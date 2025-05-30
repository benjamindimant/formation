use std::net::{IpAddr, SocketAddr, SocketAddrV4, SocketAddrV6};
use trust_dns_client::client::AsyncClient;
use trust_dns_proto::rr::rdata::CNAME;
use trust_dns_server::authority::{
    Authority, LookupOptions, UpdateResult, ZoneType, LookupError, MessageRequest,
    UpdateRequest
};
use trust_dns_proto::op::ResponseCode;
use trust_dns_proto::rr::{
    RecordType, RData, Record, RecordSet, LowerName, Name
};
use trust_dns_server::authority::LookupObject;
use crate::store::{FormDnsRecord, SharedStore, VerificationStatus};
use anyhow::Result;
use trust_dns_client::client::ClientHandle;
use crate::health::SharedIpHealthRepository;

#[derive(Clone)]
pub struct SimpleLookup {
    records: RecordSet,
    additionals: Option<RecordSet>,
}

impl SimpleLookup {
    pub fn from_record_set(rrset: RecordSet) -> Self {
        Self { records: rrset, additionals: None }
    }

    pub fn with_additionals(rrset: RecordSet, additionals: RecordSet) -> Self {
        Self { records: rrset, additionals: Some(additionals) }
    }
}

pub struct FormAuthority {
    origin: LowerName,
    zone_type: ZoneType,
    store: SharedStore,
    fallback_client: AsyncClient,
    health_repository: Option<SharedIpHealthRepository>,
}

impl FormAuthority {
    pub fn new(origin: Name, store: SharedStore, fallback_client: AsyncClient) -> Self {
        let lower_origin = LowerName::new(&origin);
        Self {
            origin: lower_origin,
            zone_type: ZoneType::Primary,
            store,
            fallback_client,
            health_repository: None,
        }
    }

    /// Configure the authority with a health repository for filtering unhealthy IPs
    pub fn with_health_repository(mut self, repository: SharedIpHealthRepository) -> Self {
        self.health_repository = Some(repository);
        self
    }

    async fn lookup_local(
        &self,
        name: &str,
        rtype: RecordType,
        src: Option<IpAddr>,
    ) -> Option<RecordSet> {
        log::info!("trimming name");
        let key = name.trim_end_matches('.').to_lowercase();
        log::info!("trimmed name: {key}");

        let record_opt = {
            let guard = self.store.read().await;
            guard.get(&key)
        };
        log::info!("retrieved record {record_opt:?}");

        if let Some(record) = record_opt {
            let is_formnet = {
                match src {
                    Some(IpAddr::V4(addr)) => addr.octets()[0] == 10,
                    Some(IpAddr::V6(_)) => false,
                    None => false,
                }
            };
            log::info!("Request is formnet? {is_formnet}");
            let mut ips = if is_formnet {
                if !record.formnet_ip.is_empty() {
                    let mut ips = record.formnet_ip.clone();
                    if !record.public_ip.is_empty() {
                        ips.extend(record.public_ip.clone());
                    }
                    ips
                } else if !record.public_ip.is_empty() {
                    record.public_ip.clone()
                } else {
                    vec![]
                }
            } else {
                if !record.public_ip.is_empty() {
                    record.public_ip.clone()
                } else {
                    vec![]
                }
            };

            // Filter out unhealthy IPs if health repository is configured
            if let Some(health_repo) = &self.health_repository {
                let original_count = ips.len();
                
                // Extract IPs without port for health check
                let ip_addrs: Vec<IpAddr> = ips.iter().map(|addr| addr.ip()).collect();
                
                // Get filtered IPs based on health status
                let health_repo_guard = health_repo.read().await;
                let filtered_ips = health_repo_guard.filter_available_ips(&ip_addrs);
                
                if filtered_ips.len() < ip_addrs.len() {
                    log::info!(
                        "Health filtering: removed {} unhealthy IPs, {} remaining",
                        ip_addrs.len() - filtered_ips.len(),
                        filtered_ips.len()
                    );
                    
                    // Only keep socket addresses with healthy IPs
                    let filtered_socket_addrs: Vec<SocketAddr> = ips
                        .into_iter()
                        .filter(|socket_addr| filtered_ips.contains(&socket_addr.ip()))
                        .collect();
                    
                    ips = filtered_socket_addrs;
                }
                
                // If no healthy IPs remain, log a warning but continue with the original set
                if ips.is_empty() && original_count > 0 {
                    log::warn!(
                        "Health filtering removed all IPs for {}. Using all IPs anyway to avoid service disruption.",
                        key
                    );
                    // Re-extract the original IPs to avoid complete service disruption
                    ips = if is_formnet {
                        if !record.formnet_ip.is_empty() {
                            let mut ips = record.formnet_ip.clone();
                            if !record.public_ip.is_empty() {
                                ips.extend(record.public_ip.clone());
                            }
                            ips
                        } else if !record.public_ip.is_empty() {
                            record.public_ip.clone()
                        } else {
                            vec![]
                        }
                    } else {
                        if !record.public_ip.is_empty() {
                            record.public_ip.clone()
                        } else {
                            vec![]
                        }
                    };
                }
            }
            
            // If we have a source IP and IPs to sort, use geolocation to sort them
            if let Some(source_ip) = src {
                if !ips.is_empty() {
                    // Extract IPs without port
                    let ip_addrs: Vec<IpAddr> = ips.iter().map(|addr| addr.ip()).collect();
                    
                    // Sort IPs by proximity to client
                    let sorted_ips = crate::geo_util::sort_ips_by_client_location(
                        &key, 
                        rtype,
                        Some(source_ip),
                        ip_addrs.clone()
                    );
                    
                    // If successfully sorted, reorder the original SocketAddrs based on sorted IPs
                    if sorted_ips.len() == ip_addrs.len() {
                        // Create a map of IP to original SocketAddr to preserve ports
                        let addr_map: std::collections::HashMap<IpAddr, SocketAddr> = 
                            ips.iter().map(|addr| (addr.ip(), *addr)).collect();
                        
                        // Rebuild socket addresses in the sorted order
                        ips = sorted_ips.into_iter()
                            .filter_map(|ip| addr_map.get(&ip).cloned())
                            .collect();
                        
                        log::info!("IPs sorted by geolocation: {ips:?}");
                    }
                }
            }

            log::info!("Final IPS: {ips:?}");

            // Calculate TTL based on health status
            let ttl = match &self.health_repository {
                Some(_) => {
                    // Use a lower TTL when health filtering is active to allow for faster recovery
                    60 // 1 minute TTL when health filtering is active
                },
                None => {
                    // Use a higher TTL when not doing health checks
                    300 // 5 minutes TTL by default
                }
            };

            if let Ok(rr_name) = Name::from_utf8(&key) {
                let mut rrset = RecordSet::new(&rr_name, rtype, ttl);
                match rtype {
                    RecordType::A => {
                        for ip in ips { 
                            if let IpAddr::V4(v4) = ip.ip() {
                                let mut rec = Record::with(rrset.name().clone(), RecordType::A, ttl);
                                rec.set_data(Some(trust_dns_proto::rr::rdata::A(v4)));
                                rrset.add_rdata(rec.into_record_of_rdata().data()?.clone());
                            }
                        }
                    }
                    RecordType::AAAA => {
                        for ip in ips {
                            if let IpAddr::V6(v6) = ip.ip() {
                                let mut rec = Record::with(rrset.name().clone(), RecordType::AAAA, ttl);
                                rec.set_data(Some(trust_dns_proto::rr::rdata::AAAA(v6)));
                                rrset.add_rdata(rec.into_record_of_rdata().data()?.clone());
                            }
                        }
                    }
                    RecordType::CNAME => {
                        log::info!("Request is for CNAME record");
                        if let Ok(name) = Name::from_utf8(record.cname_target?) {
                            let rdata = RData::CNAME(CNAME(name));
                            let rec: Record<RData> = Record::from_rdata(rrset.name().clone(), ttl, rdata);
                            rrset.insert(rec, ttl);
                        }
                    }
                    _ => {}
                }

                if !rrset.is_empty() {
                    return Some(rrset);
                }
            }
        }

        None
    }

    async fn lookup_upstream(
        &self,
        name: &LowerName,
        rtype: RecordType,
    ) -> Result<RecordSet, LookupError> {
        let fqdn_name = Name::from_utf8(&name.to_string())
            .map_err(|_| LookupError::ResponseCode(ResponseCode::FormErr))?;

        let mut client = self.fallback_client.clone();
        let response = client.query(
            fqdn_name.clone(),
            trust_dns_proto::rr::DNSClass::IN,
            rtype
        ).await.map_err(|_| LookupError::ResponseCode(ResponseCode::ServFail))?;

        let answers = response.answers();
        if answers.is_empty() {
            return Err(LookupError::ResponseCode(ResponseCode::NXDomain));
        }

        let mut rrset = RecordSet::new(&fqdn_name, rtype, 300);
        for ans in answers {
            if ans.record_type() == rtype {
                rrset.add_rdata(
                    ans.clone()
                        .into_record_of_rdata()
                        .data()
                        .ok_or(
                            LookupError::ResponseCode(
                                ResponseCode::FormErr
                            )
                        )?
                        .clone()
                );
            }
        }

        if rrset.is_empty() {
            return Err(LookupError::ResponseCode(ResponseCode::NXDomain));
        }

        Ok(rrset)
    }

    async fn lookup_fallback(
        &self,
        name: &LowerName,
        rtype: RecordType,
    ) -> Result<RecordSet, LookupError> {
        self.lookup_upstream(name, rtype).await
    }

    async fn apply_update(&self, msg: &MessageRequest) -> Result<bool, ResponseCode> {
        let _zone_name = msg.query().name();

        let updates = msg.updates();
        if updates.is_empty() {
            return Ok(false)
        }

        let mut changed = false;

        let mut store_guard = self.store.write().await;

        for rec in updates {
            let domain = rec.name().to_string().to_lowercase();
            let rtype = rec.record_type();
            let ttl = rec.ttl();

            match (rtype, rec.clone().into_record_of_rdata().data()) {
                (RecordType::A, Some(&RData::A(v4))) => {
                    if ttl == 0 {
                        if store_guard.remove(&domain).is_some() {
                            changed = true;
                        } else {
                            let record = FormDnsRecord {
                                domain: domain.clone(),
                                record_type: rtype,
                                formnet_ip: if v4.octets()[0] == 10 {
                                    vec![SocketAddr::V4(SocketAddrV4::new(v4.into(), 80))]
                                } else {
                                    vec![]
                                },
                                public_ip: if v4.octets()[0] != 10 {
                                    vec![SocketAddr::V4(SocketAddrV4::new(v4.into(), 80))]
                                } else {
                                    vec![]
                                },
                                cname_target: None,
                                ssl_cert: false,
                                ttl: 3600,
                                verification_status: Some(VerificationStatus::NotVerified),
                                verification_timestamp: None,
                            };
                            store_guard.insert(&domain, record).await;
                            changed = true;
                        }
                    } else {
                        if let Some(mut record) = store_guard.get(&domain) {
                            let form_record = FormDnsRecord {
                                record_type: rtype,
                                formnet_ip: if v4.octets()[0] == 10 {
                                    record.formnet_ip.push(SocketAddr::V4(SocketAddrV4::new(v4.into(), 80)));
                                    record.formnet_ip.clone()
                                } else { 
                                    record.formnet_ip.clone()
                                },
                                public_ip: if v4.octets()[0] != 10 {
                                    record.public_ip.push(SocketAddr::V4(SocketAddrV4::new(v4.into(), 80)));
                                    record.public_ip.clone()
                                } else {
                                    vec![]
                                },
                                ttl,
                                ..record
                            };
                            store_guard.insert(&domain, form_record).await;
                            changed = true;
                        } else {
                            let record = FormDnsRecord {
                                domain: domain.clone(),
                                record_type: rtype,
                                formnet_ip: if v4.octets()[0] == 10 {
                                    vec![SocketAddr::V4(SocketAddrV4::new(v4.into(), 80))]
                                } else {
                                    vec![]
                                },
                                public_ip: if v4.octets()[0] != 10 {
                                    vec![SocketAddr::V4(SocketAddrV4::new(v4.into(), 80))]
                                } else {
                                    vec![]
                                },
                                ssl_cert: false,
                                cname_target: None,
                                ttl: 3600,
                                verification_status: Some(VerificationStatus::NotVerified),
                                verification_timestamp: None,
                            };
                            store_guard.insert(&domain, record).await;
                            changed = true;
                        }
                    }
                },
                (RecordType::AAAA, Some(&RData::AAAA(v6))) => {
                    if ttl == 0 {
                        if store_guard.remove(&domain).is_some() {
                            changed = true;
                        } else {
                            let record = FormDnsRecord {
                                domain: domain.clone(),
                                record_type: rtype,
                                formnet_ip: vec![],
                                public_ip: vec![SocketAddr::V6(SocketAddrV6::new(v6.into(), 80, 0, 0))],
                                cname_target: None,
                                ssl_cert: false,
                                ttl: 3600,
                                verification_status: Some(VerificationStatus::NotVerified),
                                verification_timestamp: None,
                            };
                            store_guard.insert(&domain, record).await;
                            changed = true;
                        }
                    } else {
                        if let Some(mut record) = store_guard.get(&domain) {
                            let form_record = FormDnsRecord {
                                record_type: rtype,
                                formnet_ip: vec![],
                                public_ip: {
                                    record.public_ip.push(SocketAddr::V6(SocketAddrV6::new(v6.into(), 80, 0, 0)));
                                    record.public_ip
                                },
                                ttl,
                                ..record
                            };
                            store_guard.insert(&domain, form_record).await;
                            changed = true;
                        } else {
                            let record = FormDnsRecord {
                                domain: domain.clone(),
                                record_type: rtype,
                                formnet_ip: vec![],
                                public_ip: vec![SocketAddr::V6(SocketAddrV6::new(v6.into(), 80, 0, 0))],
                                cname_target: None,
                                ssl_cert: false,
                                ttl: 3600,
                                verification_status: Some(VerificationStatus::NotVerified),
                                verification_timestamp: None,
                            };
                            store_guard.insert(&domain, record).await;
                            changed = true;
                        }
                    }
                }
                (RecordType::CNAME, Some(&RData::CNAME(ref target))) => {
                    if ttl == 0 {
                        if store_guard.remove(&domain).is_some() {
                            changed = true;
                        } else {
                            let record = FormDnsRecord {
                                domain: domain.clone(),
                                record_type: rtype,
                                formnet_ip: vec![],
                                public_ip: vec![],
                                cname_target: Some(target.0.to_string()),
                                ssl_cert: false,
                                ttl: 3600,
                                verification_status: Some(VerificationStatus::NotVerified),
                                verification_timestamp: None,
                            };
                            store_guard.insert(&domain, record).await;
                        }
                    } else {
                        if let Some(record) = store_guard.get(&domain) {
                            let form_record = FormDnsRecord {
                                record_type: rtype,
                                cname_target: Some(target.0.to_string()),
                                ttl,
                                ..record
                            };
                            store_guard.insert(&domain, form_record).await;
                            changed = true;
                        } else {
                            let record = FormDnsRecord {
                                domain: domain.clone(),
                                record_type: rtype,
                                formnet_ip: vec![],
                                public_ip: vec![],
                                cname_target: Some(target.0.to_string()),
                                ssl_cert: false,
                                ttl: 3600,
                                verification_status: Some(VerificationStatus::NotVerified),
                                verification_timestamp: None,
                            };
                            store_guard.insert(&domain, record).await;
                            changed = true;
                        }
                    }
                }
                _ => {
                }
            }
        }

        Ok(changed)
    }
}

impl LookupObject for SimpleLookup {
    fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    fn iter(&self) -> Box<dyn Iterator<Item = &'_ Record> + Send + '_> {
        Box::new(
            self.records.records_without_rrsigs()
        )
    }

    fn take_additionals(&mut self) -> Option<Box<dyn LookupObject>> {
        if let Some(adds) = self.additionals.take() {
            return Some(Box::new(SimpleLookup {
                records: adds,
                additionals: None,
            }))
        }
        None
    }
}

impl Authority for FormAuthority {
    type Lookup = SimpleLookup;

    fn zone_type(&self) -> ZoneType {
        self.zone_type
    }

    fn is_axfr_allowed(&self) -> bool {
        false
    }

    fn update<'life0,'life1,'async_trait>(&'life0 self,update: &'life1 MessageRequest) ->  ::core::pin::Pin<Box<dyn ::core::future::Future<Output = UpdateResult<bool> > + ::core::marker::Send+'async_trait> >where 'life0:'async_trait,'life1:'async_trait,Self:'async_trait {
        Box::pin(async move {
            match self.apply_update(update).await {
                Ok(changed) => Ok(changed),
                Err(rcode) => Err(rcode.into())

            }
        })
    }

    fn origin(&self) ->  &LowerName {
        &self.origin
    }

    fn lookup<'life0,'life1,'async_trait>(&'life0 self,name: &'life1 LowerName,rtype:RecordType,_lookup_options:LookupOptions,) ->  ::core::pin::Pin<Box<dyn ::core::future::Future<Output = std::result::Result<Self::Lookup,LookupError> > + ::core::marker::Send+'async_trait> >where 'life0:'async_trait,'life1:'async_trait,Self:'async_trait {
        Box::pin(async move {
            let name_str = name.to_string();
            if let Some(rrset) = self.lookup_local(&name_str, rtype, None).await {
                return Ok(SimpleLookup::from_record_set(rrset));
            }

            match self.lookup_fallback(name, rtype).await {
                Ok(rr) => Ok(SimpleLookup::from_record_set(rr)),
                Err(e) => Err(e),
            }
        })
    }

    fn search<'life0,'life1,'async_trait>(&'life0 self,request:trust_dns_server::server::RequestInfo<'life1> ,_lookup_options:LookupOptions,) ->  ::core::pin::Pin<Box<dyn ::core::future::Future<Output = std::result::Result<Self::Lookup,LookupError> > + ::core::marker::Send+'async_trait> >where 'life0:'async_trait,'life1:'async_trait,Self:'async_trait {
        Box::pin(async move {
            let src = request.src;
            let rtype = request.query.query_type();
            let name = request.query.name();
            if let Some(rrset) = self.lookup_local(&name.to_string(), rtype, Some(src.ip())).await {
                log::info!("Found record in local, returning...");
                return Ok(SimpleLookup::from_record_set(rrset));
            }

            log::info!("Unable to find record in checking fallback...");
            match self.lookup_fallback(name.into(), rtype).await {
                Ok(rr) => Ok(SimpleLookup::from_record_set(rr)),
                Err(e) => Err(e),
            }
        })
    }

    fn get_nsec_records<'life0,'life1,'async_trait>(&'life0 self,_name: &'life1 LowerName,_lookup_options:LookupOptions,) ->  ::core::pin::Pin<Box<dyn ::core::future::Future<Output = std::result::Result<Self::Lookup,LookupError> > + ::core::marker::Send+'async_trait> >where 'life0:'async_trait,'life1:'async_trait,Self:'async_trait {
        Box::pin(async move {
            Err(LookupError::ResponseCode(ResponseCode::NXDomain))
        })
    }
}
