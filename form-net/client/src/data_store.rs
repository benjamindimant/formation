use shared::Error;
use anyhow::bail;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use shared::{chmod, ensure_dirs_exist, Cidr, IoErrorContext, Peer, WrappedIoError};
use std::{
    fmt::Display, fs::{File, OpenOptions}, io::{self, Read, Seek, Write}, path::{Path, PathBuf}
};
use wireguard_control::InterfaceName;

#[derive(Debug)]
pub struct DataStore<T: Display + Clone + PartialEq + Serialize + DeserializeOwned> {
    file: File,
    contents: Contents<T>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "version")]
pub enum Contents<T: Display + Clone + PartialEq> {
    #[serde(rename = "1")]
    V1 { peers: Vec<Peer<T>>, cidrs: Vec<Cidr<T>> },
}

impl<T: Display + Clone + PartialEq + Serialize + DeserializeOwned> DataStore<T> {
    pub(self) fn open_with_path<P: AsRef<Path>>(
        path: P,
        create: bool,
    ) -> Result<Self, WrappedIoError> {
        let path = path.as_ref();
        let is_existing_file = path.exists();

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .open(path)
            .with_path(path)?;

        if is_existing_file {
            shared::warn_on_dangerous_mode(path).with_path(path)?;
        } else {
            chmod(&file, 0o600).with_path(path)?;
        }

        let mut json = String::new();
        file.read_to_string(&mut json).with_path(path)?;
        let contents = serde_json::from_str(&json).unwrap_or_else(|_| Contents::V1 {
            peers: vec![],
            cidrs: vec![],
        });

        Ok(Self { file, contents })
    }

    pub fn get_path(data_dir: &Path, interface: &InterfaceName) -> PathBuf {
        data_dir.join(interface.to_string()).with_extension("json")
    }

    fn _open(
        data_dir: &Path,
        interface: &InterfaceName,
        create: bool,
    ) -> Result<Self, WrappedIoError> {
        ensure_dirs_exist(&[data_dir])?;
        Self::open_with_path(Self::get_path(data_dir, interface), create)
    }

    pub fn open(data_dir: &Path, interface: &InterfaceName) -> Result<Self, WrappedIoError> {
        Self::_open(data_dir, interface, false)
    }

    pub fn open_or_create(
        data_dir: &Path,
        interface: &InterfaceName,
    ) -> Result<Self, WrappedIoError> {
        Self::_open(data_dir, interface, true)
    }

    pub fn peers(&self) -> &[Peer<T>] {
        match &self.contents {
            Contents::V1 { peers, .. } => peers,
        }
    }

    /// Add new peers to the PeerStore, never deleting old ones.
    ///
    /// This is done as a protective measure, validating that the (IP, PublicKey) tuple
    /// of the interface's peers never change, i.e. "pinning" them. This prevents a compromised
    /// server from impersonating an existing peer.
    ///
    /// Note, however, that this does not prevent a compromised server from adding a new
    /// peer under its control, of course.
    pub fn update_peers(&mut self, current_peers: &[Peer<T>]) -> Result<(), Error> {
        let peers = match &mut self.contents {
            Contents::V1 { ref mut peers, .. } => peers,
        };

        for new_peer in current_peers.iter() {
            if let Some(existing_peer) = peers.iter_mut().find(|p| p.ip == new_peer.ip) {
                if existing_peer.public_key != new_peer.public_key {
                    log::error!(
                        "PEER IP: {}, existing peer public key: {} new peer public key: {}",
                        existing_peer.ip, existing_peer.public_key, new_peer.public_key
                    );
                    bail!("PINNING ERROR: New peer has same IP but different public key.");
                } else {
                    *existing_peer = new_peer.clone();
                }
            } else {
                peers.push(new_peer.clone());
            }
        }

        for existing_peer in peers.iter_mut() {
            if !current_peers
                .iter()
                .any(|p| p.public_key == existing_peer.public_key)
            {
                existing_peer.contents.is_disabled = true;
            }
        }

        Ok(())
    }

    pub fn cidrs(&self) -> &[Cidr<T>] {
        match &self.contents {
            Contents::V1 { cidrs, .. } => cidrs,
        }
    }

    pub fn set_cidrs(&mut self, new_cidrs: Vec<Cidr<T>>) {
        match &mut self.contents {
            Contents::V1 { ref mut cidrs, .. } => *cidrs = new_cidrs,
        }
    }

    pub fn write(&mut self) -> Result<(), io::Error> {
        self.file.rewind()?;
        self.file.set_len(0)?;
        self.file
            .write_all(serde_json::to_string_pretty(&self.contents)?.as_bytes())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::Lazy;
    use shared::{Cidr, CidrContents, Peer, PeerContents};
    static BASE_PEERS: Lazy<Vec<Peer<i64>>> = Lazy::new(|| {
        vec![Peer {
            id: 0,
            contents: PeerContents {
                name: "blah".parse().unwrap(),
                ip: "10.0.0.1".parse().unwrap(),
                cidr_id: 1,
                public_key: "abc".to_string(),
                endpoint: None,
                is_admin: false,
                is_disabled: false,
                is_redeemed: true,
                persistent_keepalive_interval: None,
                invite_expires: None,
                candidates: vec![],
            },
        }]
    });
    static BASE_CIDRS: Lazy<Vec<Cidr<i64>>> = Lazy::new(|| {
        vec![Cidr {
            id: 1,
            contents: CidrContents {
                name: "cidr".to_string(),
                cidr: "10.0.0.0/24".parse().unwrap(),
                parent: None,
            },
        }]
    });

    fn setup_basic_store(dir: &Path) {
        let mut store = DataStore::open_with_path(dir.join("peer_store.json"), true).unwrap();

        println!("{store:?}");
        assert_eq!(0, store.peers().len());
        assert_eq!(0, store.cidrs().len());

        store.update_peers(&BASE_PEERS).unwrap();
        store.set_cidrs(BASE_CIDRS.to_owned());
        store.write().unwrap();
    }

    #[test]
    fn test_sanity() {
        let dir = tempfile::tempdir().unwrap();
        setup_basic_store(dir.path());
        let store = DataStore::open_with_path(dir.path().join("peer_store.json"), false).unwrap();
        assert_eq!(store.peers(), &*BASE_PEERS);
        assert_eq!(store.cidrs(), &*BASE_CIDRS);
    }

    #[test]
    fn test_pinning() {
        let dir = tempfile::tempdir().unwrap();
        setup_basic_store(dir.path());
        let mut store =
            DataStore::open_with_path(dir.path().join("peer_store.json"), false).unwrap();

        // Should work, since peer is unmodified.
        store.update_peers(&BASE_PEERS).unwrap();

        let mut modified = BASE_PEERS.clone();
        modified[0].contents.public_key = "foo".to_string();

        // Should NOT work, since peer is unmodified.
        assert!(store.update_peers(&modified).is_err());
    }

    #[test]
    fn test_peer_persistence() {
        let dir = tempfile::tempdir().unwrap();
        setup_basic_store(dir.path());
        let mut store =
            DataStore::open_with_path(dir.path().join("peer_store.json"), false).unwrap();

        // Should work, since peer is unmodified.
        store.update_peers(&[]).unwrap();
        let new_peers = BASE_PEERS
            .iter()
            .cloned()
            .map(|mut p| {
                p.contents.is_disabled = true;
                p
            })
            .collect::<Vec<_>>();
        assert_eq!(store.peers(), &new_peers);
    }
}
