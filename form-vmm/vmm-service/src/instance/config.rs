use std::net::Ipv4Addr;
use std::str::FromStr;
use std::path::PathBuf;
use form_pack::formfile::Formfile;
use net_util::MacAddr;
use serde::{Deserialize, Serialize};
use crate::error::VmmError;
use form_types::VmmEvent;
use rand::{thread_rng, Rng};
use gabble::Gab;

pub const IMAGE_DIR: &str = "/var/lib/formation/vm-images";


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmInstanceConfig {
    pub kernel_path: PathBuf,
    pub rootfs_path: PathBuf,
    pub tap_device: String,
    pub ip_addr: String,
    pub memory_mb: u64,
    pub vcpu_count: u8,
    pub name: String,
    pub custom_cmdline: Option<String>,
    pub rng_source: Option<String>,
    pub console_type: ConsoleType,
    pub formfile: String,
    pub owner: String,
    /// List of GPU device configurations
    pub gpu_devices: Option<Vec<GpuConfig>>,
}

/// Configuration for a GPU device to be passed through to a VM
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuConfig {
    /// GPU model requested (e.g., "RTX5090", "H100", "H200", "B200")
    pub model: String,
    /// Number of GPUs requested (1-8)
    pub count: u8,
    /// Actual PCI addresses of the assigned GPUs (filled by the system, not by the user)
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub assigned_devices: Vec<GpuDeviceInfo>,
}

/// Information about an assigned GPU device
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuDeviceInfo {
    /// PCI address of the GPU (e.g., "0000:01:00.0")
    pub pci_address: String,
    /// IOMMU group to which this device belongs
    pub iommu_group: Option<String>,
    /// Enable NVIDIA GPUDirect P2P DMA over PCIe
    pub enable_gpudirect: bool,
}

impl Default for VmInstanceConfig {
    fn default() -> Self {
        let mut rng = thread_rng();
        let name: Gab = rng.gen();
        Self {
            kernel_path: PathBuf::from("/var/lib/formation/kernel/hypervisor-fw"),
            rootfs_path: PathBuf::from("/var/lib/formation/vm-images/ubuntu/22.04/default/disk.raw"),
            tap_device: "vnet0".to_string(),
            ip_addr: "11.0.0.44".to_string(),
            memory_mb: 1024,
            vcpu_count: 2,
            formfile: String::new(),
            name: name.to_string(),
            custom_cmdline: None,
            rng_source: None,
            console_type: ConsoleType::Virtio,
            owner: String::new(),
            gpu_devices: None,
        }
    }
}

impl VmInstanceConfig {
    pub fn validate(&self) -> Result<(), VmmError> {
        // Validate paths exist
        if !self.kernel_path.exists() {
            return Err(VmmError::InvalidPath(
                format!("Kernel path does not exist: {:?}", self.kernel_path)
            ));
        }
        if !self.rootfs_path.exists() {
            return Err(VmmError::InvalidPath(
                format!("Rootfs path does not exist: {:?}", self.rootfs_path)
            ));
        }

        // Validate memory and CPU configuration
        if self.memory_mb < 128 {
            return Err(VmmError::Config("Memory must be at least 128MB".into()));
        }
        if self.vcpu_count == 0 {
            return Err(VmmError::Config("Must have at least 1 vCPU".into()));
        }

        // Validate GPU configurations if any are specified
        if let Some(gpu_configs) = &self.gpu_devices {
            for gpu in gpu_configs {
                // Validate model is supported
                match gpu.model.as_str() {
                    "RTX5090" | "H100" | "H200" | "B200" => {},
                    _ => {
                        return Err(VmmError::Config(
                            format!("Unsupported GPU model: {}. Supported models are: RTX5090, H100, H200, B200", gpu.model)
                        ));
                    }
                }
                
                // Validate count is between 1 and 8
                if gpu.count < 1 || gpu.count > 8 {
                    return Err(VmmError::Config(
                        format!("Invalid GPU count: {}. Must be between 1 and 8", gpu.count)
                    ));
                }
            }
        }

        Ok(())
    }

    pub fn generate_cmdline(&self) -> String {
        self.custom_cmdline.clone().unwrap_or_else(|| {
            match self.console_type {
                ConsoleType::Serial => {
                    "console=ttyS0,115200n8 earlyprintk=serial,tty0,115200 console=tty0 root=/dev/vdb1 rw virtio_pci.disable_legacy=0"
                },
                ConsoleType::Virtio => {
                    "console=hvc0 root=/dev/vdb1 rw virtio_pci.disable_legacy=0"
                },
            }.to_string()
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ConsoleType {
    Virtio,
    Serial
}

impl FromStr for ConsoleType {
    type Err = VmmError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "virtio" => Ok(Self::Virtio),
            "serial" => Ok(Self::Serial),
            _ => {
                Err(
                    VmmError::Config(
                        format!("ConsoleType {s} not a valid console type for Formation VPS instances")
                    )
                )
            }
        }
    }
}

impl TryFrom<&VmmEvent> for VmInstanceConfig {
    type Error = VmmError;
    fn try_from(event: &VmmEvent) -> Result<Self, Self::Error> {
        match &event {
            VmmEvent::Create { 
                formfile,
                name,
                owner,
                ..
            } => { 

                let rootfs_path = PathBuf::from(IMAGE_DIR).join(name).with_extension("raw"); 
                let formfile: Formfile = serde_json::from_str(&formfile).map_err(|e| {
                    VmmError::Config(e.to_string())
                })?; 
                let memory_mb = formfile.get_memory();
                let vcpu_count = formfile.get_vcpus();

                // Extract GPU device configurations from the Formfile if available
                let gpu_configs = formfile.get_gpu_devices().map(|devices| {
                    devices.iter()
                        .map(|gpu_str| {
                            // Parse the format "MODEL:COUNT"
                            let parts: Vec<&str> = gpu_str.split(':').collect();
                            let model = parts[0].to_string();
                            let count = if parts.len() > 1 {
                                parts[1].parse::<u8>().unwrap_or(1)
                            } else {
                                1
                            };
                            
                            GpuConfig {
                                model,
                                count,
                                assigned_devices: Vec::new(),
                            }
                        })
                        .collect::<Vec<GpuConfig>>()
                });

                Ok(VmInstanceConfig {
                    rootfs_path,
                    memory_mb: memory_mb.try_into().map_err(|_| {
                        VmmError::Config(
                            "unable to convert memory into u64".to_string()
                        )
                    })?,
                    vcpu_count,
                    name: name.clone(),
                    owner: owner.to_string(),
                    formfile: serde_json::to_string(&formfile).map_err(|e| VmmError::Config(e.to_string()))?,
                    gpu_devices: gpu_configs,
                    ..Default::default()
                })
            },
            _ => {
                return Err(
                    VmmError::Config(
                        format!("VmmEvent type: {event:?} cannot be converted into VmInstanceConfig")
                    )
                )
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    pub ip: Option<Ipv4Addr>,
    pub mac_addr: Option<MacAddr>,
    pub mtu: Option<u16>,
    pub tap_name: Option<String>,
    pub offload_tso: bool,
    pub offload_ufo: bool,
    pub offload_csum: bool,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            ip: None,  // Will be assigned by DHCP
            mac_addr: None,  // Will be generated if not provided
            mtu: Some(1500),
            tap_name: None,
            offload_tso: true,
            offload_ufo: true,
            offload_csum: true,
        }
    }
}
