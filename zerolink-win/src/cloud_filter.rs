use std::path::PathBuf;
use tokio::sync::mpsc::UnboundedSender;
use windows::core::PCWSTR;
use windows::Win32::Storage::CloudFilters::*;
use zerolink_core::protocol::WifiWakePayload;

pub struct CloudFilterVirtualDrive {
    sync_root_path: PathBuf,
    wake_tx: Box<UnboundedSender<WifiWakePayload>>,
    connection_key: CF_CONNECTION_KEY,
}

impl CloudFilterVirtualDrive {
    pub fn new(sync_root_path: PathBuf, wake_tx: UnboundedSender<WifiWakePayload>) -> Self {
        Self {
            sync_root_path,
            wake_tx: Box::new(wake_tx),
            connection_key: CF_CONNECTION_KEY(0),
        }
    }

    /// Initializes and registers the Virtual Sync Root using cldapi.dll.
    /// Phone photos appear in File Explorer as 0-byte virtual placeholders with zero initial disk allocation.
    pub fn register_and_connect(&mut self) -> Result<(), String> {
        // Ensure virtual sync directory exists on disk
        if !self.sync_root_path.exists() {
            let _ = std::fs::create_dir_all(&self.sync_root_path);
        }

        let path_wide: Vec<u16> = self
            .sync_root_path
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let callback_table = [
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_FETCH_DATA,
                Callback: Some(fetch_data_callback),
            },
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_CANCEL_FETCH_DATA,
                Callback: Some(cancel_fetch_data_callback),
            },
            CF_CALLBACK_REGISTRATION {
                Type: CF_CALLBACK_TYPE_NONE,
                Callback: None,
            },
        ];

        let context_ptr = self.wake_tx.as_ref() as *const UnboundedSender<WifiWakePayload> as *const std::ffi::c_void;

        unsafe {
            // Optional sync root registration for cloud provider identity
            let registration = CF_SYNC_REGISTRATION {
                StructSize: std::mem::size_of::<CF_SYNC_REGISTRATION>() as u32,
                ProviderName: PCWSTR(windows::core::w!("ZeroLink").as_ptr()),
                ProviderVersion: PCWSTR(windows::core::w!("1.0.0").as_ptr()),
                SyncRootIdentity: std::ptr::null(),
                SyncRootIdentityLength: 0,
                FileIdentity: std::ptr::null(),
                FileIdentityLength: 0,
                ProviderId: windows::core::GUID::zeroed(),
            };

            let policies = CF_SYNC_POLICIES {
                StructSize: std::mem::size_of::<CF_SYNC_POLICIES>() as u32,
                Hydration: CF_HYDRATION_POLICY {
                    Primary: CF_HYDRATION_POLICY_PRIMARY_FULL,
                    Modifier: CF_HYDRATION_POLICY_MODIFIER_NONE,
                },
                Population: CF_POPULATION_POLICY {
                    Primary: CF_POPULATION_POLICY_PRIMARY_ALWAYS_FULL,
                    Modifier: CF_POPULATION_POLICY_MODIFIER_NONE,
                },
                InSync: CF_INSYNC_POLICY_NONE,
                HardLink: CF_HARDLINK_POLICY_NONE,
                PlaceholderManagement: CF_PLACEHOLDER_MANAGEMENT_POLICY_DEFAULT,
            };

            let _ = CfRegisterSyncRoot(PCWSTR(path_wide.as_ptr()), &registration, &policies, CF_REGISTER_FLAGS_NONE);

            let hr = CfConnectSyncRoot(
                PCWSTR(path_wide.as_ptr()),
                callback_table.as_ptr(),
                context_ptr,
                CF_CONNECT_FLAGS_REQUIRE_FULL_FILE_PATH,
                &mut self.connection_key,
            );

            if hr.is_err() {
                return Err(format!("CfConnectSyncRoot failed with hr: {:?}", hr));
            }
        }

        tracing::info!(
            "Cloud Filter virtual drive connected at {:?}",
            self.sync_root_path
        );
        Ok(())
    }

    /// Creates a 0-byte virtual placeholder file for an Android photo.
    pub fn create_placeholder(
        &self,
        filename: &str,
        file_size_bytes: i64,
        file_id: &str,
    ) -> Result<(), String> {
        let name_wide: Vec<u16> = filename.encode_utf16().chain(std::iter::once(0)).collect();
        let id_bytes = file_id.as_bytes();

        let placeholder_info = CF_PLACEHOLDER_CREATE_INFO {
            RelativeFileName: PCWSTR(name_wide.as_ptr()),
            FsMetadata: CF_FS_METADATA {
                FileSize: file_size_bytes,
                BasicInfo: windows::Win32::Storage::FileSystem::FILE_BASIC_INFO::default(),
            },
            SecurityDescriptor: std::ptr::null(),
            SecurityDescriptorLength: 0,
            FileIdentity: id_bytes.as_ptr() as *const std::ffi::c_void,
            FileIdentityLength: id_bytes.len() as u32,
            Flags: CF_PLACEHOLDER_CREATE_FLAGS_NONE,
            Result: windows::core::HRESULT(0),
            CreateUsn: 0,
        };

        let path_wide: Vec<u16> = self
            .sync_root_path
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        unsafe {
            let mut entries_processed = 0u32;
            let hr = CfCreatePlaceholders(
                PCWSTR(path_wide.as_ptr()),
                &[placeholder_info],
                &mut entries_processed,
                CF_CREATE_FLAGS_NONE,
            );
            if hr.is_err() {
                return Err(format!("CfCreatePlaceholders failed: {:?}", hr));
            }
        }
        Ok(())
    }

    /// Disconnects the Cloud Filter sync root.
    pub fn disconnect(&mut self) {
        if self.connection_key.0 != 0 {
            unsafe {
                let _ = CfDisconnectSyncRoot(self.connection_key);
            }
            self.connection_key = CF_CONNECTION_KEY(0);
        }
    }
}

/// Invoked natively by the Windows OS Kernel filesystem filter driver whenever an application
/// attempts to read bytes from an unhydrated virtual placeholder file.
unsafe extern "system" fn fetch_data_callback(
    callback_info: *const CF_CALLBACK_INFO,
    callback_parameters: *const CF_CALLBACK_PARAMETERS,
) {
    if callback_info.is_null() || callback_parameters.is_null() {
        return;
    }

    let info = &*callback_info;
    let params = &*callback_parameters;

    let required_offset = params.Anonymous.FetchData.RequiredFileOffset;
    let required_length = params.Anonymous.FetchData.RequiredLength;

    tracing::info!(
        "CloudFilter fetch triggered for offset: {}, length: {}",
        required_offset,
        required_length
    );

    // If callback context contains the Wi-Fi wake transmitter, wake phone Wi-Fi
    if !info.CallbackContext.is_null() {
        let wake_sender = &*(info.CallbackContext as *const UnboundedSender<WifiWakePayload>);
        let wake_req = WifiWakePayload {
            target_ip: String::new(),
            port: 8443,
            session_token: "cldapi_fetch".to_string(),
            reason: format!("Offset:{},Len:{}", required_offset, required_length),
        };
        let _ = wake_sender.send(wake_req);
    }
}

unsafe extern "system" fn cancel_fetch_data_callback(
    _callback_info: *const CF_CALLBACK_INFO,
    _callback_parameters: *const CF_CALLBACK_PARAMETERS,
) {
    tracing::info!("CloudFilter fetch cancelled by OS/User");
}
