use std::collections::HashMap;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use tokio::sync::mpsc::UnboundedSender;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{NTSTATUS, STATUS_CLOUD_FILE_UNSUCCESSFUL};
use windows::Win32::Storage::CloudFilters::*;

#[derive(Clone, Debug)]
pub struct CloudFetchRequest {
    pub connection_key: i64,
    pub transfer_key: i64,
    pub request_key: i64,
    pub file_id: String,
    pub required_offset: i64,
    pub required_length: i64,
    pub offset: i64,
    pub length: u32,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CloudFetchIdentity {
    pub connection_key: i64,
    pub transfer_key: i64,
    pub request_key: i64,
}

#[derive(Clone, Copy, Debug)]
pub struct CloudFetchCancellation {
    pub identity: CloudFetchIdentity,
    pub offset: i64,
    pub length: i64,
}

#[derive(Clone, Debug)]
pub enum CloudFilterEvent {
    Fetch(CloudFetchRequest),
    Cancel(CloudFetchCancellation),
}

struct CallbackContext {
    event_sender: UnboundedSender<CloudFilterEvent>,
}

impl CloudFetchRequest {
    pub fn identity(&self) -> CloudFetchIdentity {
        CloudFetchIdentity {
            connection_key: self.connection_key,
            transfer_key: self.transfer_key,
            request_key: self.request_key,
        }
    }

    pub fn is_fully_cancelled_by(&self, cancellation: &CloudFetchCancellation) -> bool {
        self.identity() == cancellation.identity
            && range_contains(
                cancellation.offset,
                cancellation.length,
                self.required_offset,
                self.required_length,
            )
    }
}

fn range_contains(
    outer_offset: i64,
    outer_length: i64,
    inner_offset: i64,
    inner_length: i64,
) -> bool {
    if outer_offset < 0 || outer_length <= 0 || inner_offset < 0 || inner_length <= 0 {
        return false;
    }
    let Some(outer_end) = outer_offset.checked_add(outer_length) else {
        return false;
    };
    let Some(inner_end) = inner_offset.checked_add(inner_length) else {
        return false;
    };
    outer_offset <= inner_offset && outer_end >= inner_end
}

pub struct CloudFilterVirtualDrive {
    sync_root_path: PathBuf,
    callback_context: Box<CallbackContext>,
    file_paths: HashMap<String, PathBuf>,
    connection_key: CF_CONNECTION_KEY,
    callback_table: Box<[CF_CALLBACK_REGISTRATION]>,
}

#[allow(dead_code)]
impl CloudFilterVirtualDrive {
    pub fn new(sync_root_path: PathBuf, event_sender: UnboundedSender<CloudFilterEvent>) -> Self {
        Self {
            sync_root_path,
            callback_context: Box::new(CallbackContext { event_sender }),
            file_paths: HashMap::new(),
            connection_key: CF_CONNECTION_KEY(0),
            callback_table: Box::new([
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
            ]),
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

        let context_ptr =
            self.callback_context.as_ref() as *const CallbackContext as *const std::ffi::c_void;

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
                    Primary: CF_HYDRATION_POLICY_FULL,
                    Modifier: CF_HYDRATION_POLICY_MODIFIER_NONE,
                },
                Population: CF_POPULATION_POLICY {
                    Primary: CF_POPULATION_POLICY_ALWAYS_FULL,
                    Modifier: CF_POPULATION_POLICY_MODIFIER_NONE,
                },
                InSync: CF_INSYNC_POLICY_NONE,
                HardLink: CF_HARDLINK_POLICY_NONE,
                PlaceholderManagement: CF_PLACEHOLDER_MANAGEMENT_POLICY_DEFAULT,
            };

            let _ = CfRegisterSyncRoot(
                PCWSTR(path_wide.as_ptr()),
                &registration,
                &policies,
                CF_REGISTER_FLAG_UPDATE,
            );

            match CfConnectSyncRoot(
                PCWSTR(path_wide.as_ptr()),
                self.callback_table.as_ptr(),
                Some(context_ptr),
                CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH,
            ) {
                Ok(key) => {
                    self.connection_key = key;
                }
                Err(e) => {
                    return Err(format!("CfConnectSyncRoot failed with error: {:?}", e));
                }
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
        &mut self,
        filename: &str,
        file_size_bytes: i64,
        file_id: &str,
    ) -> Result<(), String> {
        if file_size_bytes < 0 {
            return Err("Cloud-file size cannot be negative".into());
        }
        if file_id.is_empty() || file_id.len() > 256 || file_id.as_bytes().contains(&0) {
            return Err("Invalid cloud-file identity".into());
        }
        let relative_path = validate_relative_path(filename)?;
        let normalized = relative_path.to_string_lossy().replace('/', "\\");
        let (base_dir, leaf_name) = if let Some(pos) = normalized.rfind('\\') {
            let parent_dir = self.sync_root_path.join(&normalized[..pos]);
            std::fs::create_dir_all(&parent_dir)
                .map_err(|error| format!("Could not create placeholder directory: {error}"))?;
            (parent_dir, &normalized[pos + 1..])
        } else {
            (self.sync_root_path.clone(), normalized.as_str())
        };

        let name_wide: Vec<u16> = leaf_name.encode_utf16().chain(std::iter::once(0)).collect();
        let id_bytes = file_id.as_bytes();

        let mut placeholder_info = [CF_PLACEHOLDER_CREATE_INFO {
            RelativeFileName: PCWSTR(name_wide.as_ptr()),
            FsMetadata: CF_FS_METADATA {
                FileSize: file_size_bytes,
                BasicInfo: windows::Win32::Storage::FileSystem::FILE_BASIC_INFO::default(),
            },
            FileIdentity: id_bytes.as_ptr() as *const std::ffi::c_void,
            FileIdentityLength: id_bytes.len() as u32,
            Flags: CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC,
            Result: windows::core::HRESULT(0),
            CreateUsn: 0,
        }];

        let path_wide: Vec<u16> = base_dir
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        unsafe {
            let mut entries_processed = 0u32;
            let hr = CfCreatePlaceholders(
                PCWSTR(path_wide.as_ptr()),
                &mut placeholder_info,
                CF_CREATE_FLAG_NONE,
                Some(&mut entries_processed),
            );
            if hr.is_err() {
                return Err(format!("CfCreatePlaceholders failed: {:?}", hr));
            }
        }
        self.file_paths
            .insert(file_id.to_string(), self.sync_root_path.join(relative_path));
        Ok(())
    }

    /// Returns a reference to the virtual sync root path on disk.
    pub fn sync_root_path(&self) -> &PathBuf {
        &self.sync_root_path
    }

    /// Hydrates a placeholder file with fetched data bytes from the Android device.
    pub fn hydrate_file_chunk(
        &self,
        file_id: &str,
        offset: u64,
        data: &[u8],
    ) -> Result<(), String> {
        use std::io::{Seek, SeekFrom, Write};
        let target_path = self
            .file_paths
            .get(file_id)
            .ok_or_else(|| "Unknown cloud-file identity".to_string())?;

        if let Some(parent) = target_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("Could not create hydration directory: {error}"))?;
        }

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(target_path)
            .map_err(|e| format!("Failed to open file for hydration: {}", e))?;

        file.seek(SeekFrom::Start(offset))
            .map_err(|e| format!("Failed to seek file: {}", e))?;

        file.write_all(data)
            .map_err(|e| format!("Failed to write chunk data: {}", e))?;

        file.flush()
            .map_err(|e| format!("Failed to flush hydrated file: {}", e))?;

        tracing::info!(
            "Hydrated {} bytes for file {:?} at offset {}",
            data.len(),
            target_path,
            offset
        );
        Ok(())
    }

    pub fn complete_fetch(&self, request: &CloudFetchRequest, data: &[u8]) -> Result<(), String> {
        execute_transfer(request, data, NTSTATUS(0))
    }

    pub fn fail_fetch(&self, request: &CloudFetchRequest) -> Result<(), String> {
        execute_transfer(request, &[], STATUS_CLOUD_FILE_UNSUCCESSFUL)
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

fn validate_relative_path(relative_path: &str) -> Result<PathBuf, String> {
    if relative_path.is_empty() || relative_path.contains(':') || relative_path.contains('\0') {
        return Err("Invalid relative cloud-file path".into());
    }
    let path = Path::new(relative_path);
    if path.is_absolute()
        || path.components().any(|component| {
            !matches!(component, Component::Normal(_))
                || component.as_os_str().to_string_lossy().ends_with('.')
                || component.as_os_str().to_string_lossy().ends_with(' ')
        })
    {
        return Err("Cloud-file path must stay inside the sync root".into());
    }
    Ok(path.to_path_buf())
}

fn validated_transfer_length(
    request: &CloudFetchRequest,
    data: &[u8],
    completion_status: NTSTATUS,
) -> Result<i64, String> {
    if request.length == 0 {
        return Err("Cloud Files transfer range must have a positive length".into());
    }
    if completion_status.0 == 0 && data.len() != request.length as usize {
        return Err(format!(
            "Cloud Files transfer expected {} bytes, received {}",
            request.length,
            data.len()
        ));
    }
    Ok(request.length as i64)
}

fn execute_transfer(
    request: &CloudFetchRequest,
    data: &[u8],
    completion_status: NTSTATUS,
) -> Result<(), String> {
    let transfer_length = validated_transfer_length(request, data, completion_status)?;

    let operation_info = CF_OPERATION_INFO {
        StructSize: std::mem::size_of::<CF_OPERATION_INFO>() as u32,
        Type: CF_OPERATION_TYPE_TRANSFER_DATA,
        ConnectionKey: CF_CONNECTION_KEY(request.connection_key),
        TransferKey: request.transfer_key,
        CorrelationVector: std::ptr::null(),
        SyncStatus: std::ptr::null(),
        RequestKey: request.request_key,
    };
    let mut operation_parameters = CF_OPERATION_PARAMETERS {
        ParamSize: std::mem::size_of::<CF_OPERATION_PARAMETERS>() as u32,
        ..Default::default()
    };
    operation_parameters.Anonymous.TransferData = CF_OPERATION_PARAMETERS_0_6 {
        Flags: CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
        CompletionStatus: completion_status,
        Buffer: if data.is_empty() {
            std::ptr::null()
        } else {
            data.as_ptr() as *const std::ffi::c_void
        },
        Offset: request.offset,
        Length: transfer_length,
    };
    let result = unsafe { CfExecute(&operation_info, &mut operation_parameters) };
    let audit_result = match &result {
        Ok(()) => "success".to_string(),
        Err(error) => format!("error={error}"),
    };
    write_hydration_trace(&format!(
        "Transfer offset={} requested_bytes={} payload_bytes={} status=0x{:08X} param_size={} result={audit_result}",
        request.offset,
        request.length,
        data.len(),
        completion_status.0 as u32,
        operation_parameters.ParamSize,
    ));
    result.map_err(|error| format!("CfExecute transfer failed: {error}"))
}

fn write_hydration_trace(message: &str) {
    let Ok(path) = std::env::var("ZEROLINK_TEST_HYDRATION_TRACE") else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{message}");
    }
}

fn compute_fetch_transfer_range(
    required_offset: i64,
    required_length: i64,
    optional_offset: i64,
    optional_length: i64,
    file_size: i64,
) -> Option<(i64, u32)> {
    const ALIGNMENT: i64 = 4096;
    const MAX_TRANSFER: i64 = 16 * 1024 * 1024;

    if required_offset < 0 || required_length <= 0 || file_size < 0 {
        return None;
    }

    let required_end = required_offset.checked_add(required_length)?.min(file_size);
    if required_end <= required_offset {
        return None;
    }

    let required_transfer_offset = required_offset / ALIGNMENT * ALIGNMENT;
    let mut transfer_offset = required_transfer_offset;
    let mut transfer_end = required_end;

    // OptionalLength may be CF_EOF (-1). Use the broader range only when it
    // fully covers the bytes Cloud Files says are required.
    if optional_offset >= 0 && (optional_length > 0 || optional_length == -1) {
        let optional_end = if optional_length == -1 {
            file_size
        } else {
            optional_offset.checked_add(optional_length)?.min(file_size)
        };

        if optional_offset <= required_offset && optional_end >= required_end {
            let optional_transfer_offset = optional_offset / ALIGNMENT * ALIGNMENT;
            let optional_aligned_end = optional_end
                .checked_add(ALIGNMENT - 1)?
                .checked_div(ALIGNMENT)?
                .checked_mul(ALIGNMENT)?
                .min(file_size);
            let optional_span = optional_aligned_end.saturating_sub(optional_transfer_offset);

            if optional_span <= MAX_TRANSFER {
                transfer_offset = optional_transfer_offset;
                transfer_end = optional_end;
            } else {
                let required_aligned_end = required_end
                    .checked_add(ALIGNMENT - 1)?
                    .checked_div(ALIGNMENT)?
                    .checked_mul(ALIGNMENT)?
                    .min(file_size);
                if required_aligned_end.saturating_sub(optional_transfer_offset) <= MAX_TRANSFER {
                    transfer_offset = optional_transfer_offset;
                    transfer_end = optional_transfer_offset
                        .saturating_add(MAX_TRANSFER)
                        .min(optional_end);
                } else {
                    // Keep the required bytes in the bounded transfer, then
                    // over-read toward EOF within the optional contiguous range.
                    transfer_end = required_transfer_offset
                        .saturating_add(MAX_TRANSFER)
                        .min(optional_end);
                }
            }
        }
    }

    let aligned_end = transfer_end
        .checked_add(ALIGNMENT - 1)?
        .checked_div(ALIGNMENT)?
        .checked_mul(ALIGNMENT)?
        .min(file_size);
    let transfer_length = aligned_end
        .saturating_sub(transfer_offset)
        .min(MAX_TRANSFER);
    if transfer_length <= 0 {
        return None;
    }

    Some((transfer_offset, transfer_length as u32))
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
        "CloudFilter fetch triggered for required offset: {}, length: {}; optional offset: {}, length: {}",
        required_offset,
        required_length,
        params.Anonymous.FetchData.OptionalFileOffset,
        params.Anonymous.FetchData.OptionalLength
    );
    write_hydration_trace(&format!(
        "Fetch required_offset={required_offset} required_length={required_length} optional_offset={} optional_length={} file_size={}",
        params.Anonymous.FetchData.OptionalFileOffset,
        params.Anonymous.FetchData.OptionalLength,
        info.FileSize,
    ));
    let Some((transfer_offset, transfer_length)) = compute_fetch_transfer_range(
        required_offset,
        required_length,
        params.Anonymous.FetchData.OptionalFileOffset,
        params.Anonymous.FetchData.OptionalLength,
        info.FileSize,
    ) else {
        write_hydration_trace("Could not derive a valid transfer range from FETCH_DATA parameters");
        return;
    };

    let failure_request = CloudFetchRequest {
        connection_key: info.ConnectionKey.0,
        transfer_key: info.TransferKey,
        request_key: info.RequestKey,
        file_id: String::new(),
        required_offset,
        required_length,
        offset: transfer_offset,
        length: transfer_length,
    };

    let Some(file_identity) =
        (!info.FileIdentity.is_null() && info.FileIdentityLength > 0).then(|| {
            std::slice::from_raw_parts(
                info.FileIdentity.cast::<u8>(),
                info.FileIdentityLength as usize,
            )
        })
    else {
        let _ = execute_transfer(&failure_request, &[], STATUS_CLOUD_FILE_UNSUCCESSFUL);
        return;
    };
    let Ok(file_id) = std::str::from_utf8(file_identity) else {
        let _ = execute_transfer(&failure_request, &[], STATUS_CLOUD_FILE_UNSUCCESSFUL);
        return;
    };
    if info.CallbackContext.is_null() || file_id.len() > 256 {
        let _ = execute_transfer(&failure_request, &[], STATUS_CLOUD_FILE_UNSUCCESSFUL);
        return;
    }
    let request = CloudFetchRequest {
        connection_key: info.ConnectionKey.0,
        transfer_key: info.TransferKey,
        request_key: info.RequestKey,
        file_id: file_id.to_string(),
        required_offset,
        required_length,
        offset: transfer_offset,
        length: transfer_length,
    };
    let callback_context = &*(info.CallbackContext as *const CallbackContext);
    if callback_context
        .event_sender
        .send(CloudFilterEvent::Fetch(request.clone()))
        .is_err()
    {
        let _ = execute_transfer(&request, &[], STATUS_CLOUD_FILE_UNSUCCESSFUL);
    }
}

unsafe extern "system" fn cancel_fetch_data_callback(
    callback_info: *const CF_CALLBACK_INFO,
    callback_parameters: *const CF_CALLBACK_PARAMETERS,
) {
    if callback_info.is_null() || callback_parameters.is_null() {
        return;
    }
    let info = &*callback_info;
    if info.CallbackContext.is_null() {
        return;
    }
    let params = &*callback_parameters;
    let cancel = params.Anonymous.Cancel;
    let range = cancel.Anonymous.FetchData;
    let cancellation = CloudFetchCancellation {
        identity: CloudFetchIdentity {
            connection_key: info.ConnectionKey.0,
            transfer_key: info.TransferKey,
            request_key: info.RequestKey,
        },
        offset: range.FileOffset,
        length: range.Length,
    };
    tracing::info!(
        "CloudFilter fetch cancelled by OS/User: offset {}, length {}, flags {}",
        cancellation.offset,
        cancellation.length,
        cancel.Flags.0
    );
    let callback_context = &*(info.CallbackContext as *const CallbackContext);
    let _ = callback_context
        .event_sender
        .send(CloudFilterEvent::Cancel(cancellation));
}

#[cfg(test)]
mod tests {
    use super::{
        compute_fetch_transfer_range, validate_relative_path, validated_transfer_length,
        CloudFetchCancellation, CloudFetchIdentity, CloudFetchRequest, CloudFilterEvent,
        CloudFilterVirtualDrive,
    };
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn cloud_file_paths_must_stay_under_the_sync_root() {
        assert!(validate_relative_path("Camera/photo.jpg").is_ok());
        assert!(validate_relative_path("..\\outside.txt").is_err());
        assert!(validate_relative_path("Camera/../../outside.txt").is_err());
        assert!(validate_relative_path("C:\\outside.txt").is_err());
        assert!(validate_relative_path("Camera/photo.jpg:secret").is_err());
    }

    #[test]
    fn cloud_file_fetch_uses_the_optional_contiguous_range() {
        assert_eq!(
            compute_fetch_transfer_range(0, 64 * 1024, 0, 4 * 1024 * 1024, 4 * 1024 * 1024),
            Some((0, 4 * 1024 * 1024))
        );
        assert_eq!(
            compute_fetch_transfer_range(64 * 1024, 64 * 1024, 0, -1, 4 * 1024 * 1024),
            Some((0, 4 * 1024 * 1024))
        );
    }

    #[test]
    fn cloud_file_fetch_keeps_required_bytes_when_optional_hint_is_too_large() {
        const MAX_TRANSFER: i64 = 16 * 1024 * 1024;
        let required_offset = 20 * 1024 * 1024;
        assert_eq!(
            compute_fetch_transfer_range(required_offset, 64 * 1024, 0, -1, 40 * 1024 * 1024,),
            Some((required_offset, MAX_TRANSFER as u32))
        );
    }

    #[test]
    fn cloud_file_fetch_ignores_invalid_or_non_covering_optional_hints() {
        assert_eq!(
            compute_fetch_transfer_range(4096, 4096, 8192, 4096, 1024 * 1024),
            Some((4096, 4096))
        );
        assert_eq!(
            compute_fetch_transfer_range(4096, 4096, -1, -1, 1024 * 1024),
            Some((4096, 4096))
        );
        assert_eq!(
            compute_fetch_transfer_range(0, 4096, 0, 0, 1024 * 1024),
            Some((0, 4096))
        );
    }

    #[test]
    fn cloud_file_fetch_clamps_and_aligns_at_eof() {
        assert_eq!(
            compute_fetch_transfer_range(4096, 4096, 0, -1, 10_003),
            Some((0, 10_003))
        );
        assert_eq!(compute_fetch_transfer_range(10_003, 1, 0, -1, 10_003), None);
    }

    #[test]
    fn failed_cloud_file_transfer_keeps_a_valid_range_length() {
        let request = CloudFetchRequest {
            connection_key: 1,
            transfer_key: 2,
            request_key: 3,
            file_id: "test".into(),
            required_offset: 4096,
            required_length: 2048,
            offset: 0,
            length: 4096,
        };
        assert_eq!(
            validated_transfer_length(&request, &[], super::STATUS_CLOUD_FILE_UNSUCCESSFUL,)
                .unwrap(),
            4096
        );
        assert!(validated_transfer_length(&request, &[0; 1], super::NTSTATUS(0)).is_err());
    }

    #[test]
    fn cloud_fetch_cancellation_requires_the_same_request_and_full_required_range() {
        let request = CloudFetchRequest {
            connection_key: 1,
            transfer_key: 2,
            request_key: 3,
            file_id: "test".into(),
            required_offset: 4096,
            required_length: 2048,
            offset: 0,
            length: 8192,
        };
        let cancellation = |identity, offset, length| CloudFetchCancellation {
            identity,
            offset,
            length,
        };

        assert!(request.is_fully_cancelled_by(&cancellation(request.identity(), 0, 8192)));
        assert!(!request.is_fully_cancelled_by(&cancellation(request.identity(), 0, 6143)));
        assert!(!request.is_fully_cancelled_by(&cancellation(
            CloudFetchIdentity {
                request_key: 4,
                ..request.identity()
            },
            0,
            8192,
        )));
        assert!(!request.is_fully_cancelled_by(&cancellation(request.identity(), i64::MAX, 2)));
    }

    #[tokio::test]
    #[ignore = "requires the Windows Cloud Files API; run explicitly for the local hydration probe"]
    async fn cloud_files_callback_hydrates_a_local_placeholder() {
        use std::os::windows::ffi::OsStrExt;
        use tokio::sync::mpsc;
        use tokio::time::timeout;
        use windows::core::PCWSTR;
        use windows::Win32::Storage::CloudFilters::CfUnregisterSyncRoot;

        const FILE_SIZE: usize = 4 * 1024 * 1024;
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .canonicalize()
            .expect("canonical workspace root");
        let run_nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after UNIX epoch")
            .as_nanos();
        let sync_root = workspace_root.join(".codex-target").join(format!(
            "cloud-filter-local-probe-{}-{run_nonce}",
            std::process::id()
        ));
        assert!(sync_root.starts_with(&workspace_root));
        assert!(!sync_root.exists(), "probe sync root must be unique");

        let expected: Vec<u8> = (0..FILE_SIZE)
            .map(|index| (index.wrapping_mul(31).wrapping_add(17) & 0xff) as u8)
            .collect();
        let file_id = "zerolink-local-hydration-probe-v1".to_string();
        let (event_sender, mut event_receiver) = mpsc::unbounded_channel();
        let mut drive = CloudFilterVirtualDrive::new(sync_root.clone(), event_sender);
        let mut connected = false;

        let hydration_result: Result<(usize, usize), String> = async {
            drive.register_and_connect()?;
            connected = true;
            drive.create_placeholder(
                "Verification\\cloud-filter-local-test.bin",
                FILE_SIZE as i64,
                &file_id,
            )?;

            let file_path = drive
                .sync_root_path()
                .join("Verification")
                .join("cloud-filter-local-test.bin");
            let mut read_task = tokio::task::spawn_blocking(move || std::fs::read(file_path));
            let mut transfer_count = 0usize;
            let hydrated = loop {
                tokio::select! {
                    read_result = &mut read_task => {
                        let data = read_result
                            .map_err(|error| format!("Placeholder read task failed: {error}"))?
                            .map_err(|error| format!("Could not read the local placeholder: {error}"))?;
                        break data;
                    }
                    fetch_result = timeout(Duration::from_secs(15), event_receiver.recv()) => {
                        let event = fetch_result
                            .map_err(|_| "Timed out waiting for a local FETCH_DATA callback".to_string())?
                            .ok_or_else(|| "Cloud Files closed the local fetch channel".to_string())?;
                        let request = match event {
                            CloudFilterEvent::Fetch(request) => request,
                            CloudFilterEvent::Cancel(_) => continue,
                        };
                        if request.file_id != file_id {
                            return Err(format!("Unexpected local file identity: {}", request.file_id));
                        }
                        let start = usize::try_from(request.offset)
                            .map_err(|_| "Cloud Files requested a negative range offset".to_string())?;
                        let end = start
                            .checked_add(request.length as usize)
                            .ok_or_else(|| "Cloud Files requested an overflowing range".to_string())?;
                        let range = expected
                            .get(start..end)
                            .ok_or_else(|| "Cloud Files requested a range beyond the test file".to_string())?;
                        drive.complete_fetch(&request, range)?;
                        transfer_count += 1;
                    }
                }
            };

            if hydrated != expected {
                return Err("The local Cloud Files read did not match the expected bytes".into());
            }
            Ok((hydrated.len(), transfer_count))
        }
        .await;

        drive.disconnect();
        drop(drive);
        let sync_root_wide: Vec<u16> = sync_root
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let unregister_result = unsafe { CfUnregisterSyncRoot(PCWSTR(sync_root_wide.as_ptr())) };
        if connected {
            assert!(
                unregister_result.is_ok(),
                "Could not unregister local probe sync root: {unregister_result:?}"
            );
        }
        if sync_root.exists() {
            std::fs::remove_dir_all(&sync_root)
                .expect("remove only the unique local probe sync root");
        }

        let (hydrated_bytes, transfer_count) =
            hydration_result.expect("local placeholder hydration");
        assert_eq!(hydrated_bytes, FILE_SIZE);
        assert!(transfer_count > 0);
        println!("Local Cloud Files hydration passed: {hydrated_bytes} bytes in {transfer_count} range(s).");
    }
}
