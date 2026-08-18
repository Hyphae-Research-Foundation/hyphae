// SPDX-License-Identifier: Apache-2.0

//! Native product opening and local lifecycle helpers.

use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) fn logical_time_micros() -> i64 {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_micros());
    i64::try_from(micros).unwrap_or(i64::MAX)
}

pub(crate) fn default_endpoint(data_dir: &Path) -> String {
    #[cfg(unix)]
    {
        data_dir.join("hyphae.sock").to_string_lossy().into_owned()
    }
    #[cfg(windows)]
    {
        bare_pipe_identity(data_dir)
    }
}

/// Returns the bare namespace identity expected by `ToNsName` on Windows.
#[cfg(any(windows, test))]
pub(crate) fn bare_pipe_identity(data_dir: &Path) -> String {
    let mut identity = 0xcbf2_9ce4_8422_2325_u64;
    for byte in data_dir.as_os_str().as_encoded_bytes() {
        identity ^= u64::from(*byte);
        identity = identity.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("hyphae-{identity:016x}")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    #[test]
    fn windows_pipe_identity_is_bare_for_namespaced_conversion() {
        let identity = super::bare_pipe_identity(Path::new("native-data"));
        assert!(identity.starts_with("hyphae-"));
        assert!(!identity.contains('\\'));
    }
}
