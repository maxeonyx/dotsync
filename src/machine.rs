use std::ffi::OsString;
use std::fs;
use std::path::Path;

use crate::error::DotsyncError;

#[derive(Debug, Clone)]
pub(crate) struct MachineIdentity {
    pub(crate) os_scope: String,
    pub(crate) machine_scope: String,
}

pub(crate) fn detect_machine() -> Result<MachineIdentity, DotsyncError> {
    let os_scope = std::env::var("DOTSYNC_OS").unwrap_or_else(|_| detect_os().to_string());
    let machine_scope = match std::env::var("DOTSYNC_HOSTNAME") {
        Ok(hostname) => hostname,
        Err(_) => detect_hostname()?,
    };
    Ok(MachineIdentity {
        os_scope,
        machine_scope,
    })
}

pub(crate) fn detect_os() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}

pub(crate) fn detect_hostname() -> Result<String, DotsyncError> {
    if let Some(hostname) = std::env::var_os("HOSTNAME")
        .and_then(non_empty_os_string)
        .and_then(|hostname| hostname.into_string().ok())
    {
        return Ok(hostname);
    }
    if let Some(hostname) = std::env::var_os("COMPUTERNAME")
        .and_then(non_empty_os_string)
        .and_then(|hostname| hostname.into_string().ok())
    {
        return Ok(hostname);
    }
    let etc_hostname = Path::new("/etc/hostname");
    if etc_hostname.exists() {
        let hostname = fs::read_to_string(etc_hostname).map_err(|source| DotsyncError::Io {
            path: etc_hostname.to_path_buf(),
            source,
        })?;
        let hostname = hostname.trim();
        if !hostname.is_empty() {
            return Ok(hostname.to_string());
        }
    }
    Err(DotsyncError::MissingHostname)
}

pub(crate) fn non_empty_os_string(value: OsString) -> Option<OsString> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}
