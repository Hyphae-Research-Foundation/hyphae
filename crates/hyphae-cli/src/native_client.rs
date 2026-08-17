// SPDX-License-Identifier: Apache-2.0

//! Process-local CLI client over the native product dispatcher.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::Path,
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use hyphae_native_product::{
    ApiKeyId, AuthorizationEpoch, LegacyBearerMigrationActivationReceipt,
    LegacyBearerMigrationStartReceipt, MAX_API_KEY_CREDENTIAL_BYTES, NativeProduct,
    OwnerRecoveryAbortReceipt, OwnerRecoveryActivationReceipt, OwnerRecoveryInspection,
    OwnerRecoveryStartReceipt, ProductAuthorization, ProductCancellationToken, ProductDurability,
    ProductError, ProductErrorCode, ProductLimits, ProductOperation, ProductPrincipal,
    ProductResponse, ProductSession, ProductSessionId,
};
use uuid::Uuid;

use crate::{exit::CliFailure, native::logical_time_micros};

const CONSOLE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct ApiKeyBuffer(Vec<u8>);

impl ApiKeyBuffer {
    pub(crate) fn from_bytes(bytes: Vec<u8>) -> Result<Self, CliFailure> {
        let mut value = Self(bytes);
        value.normalize()?;
        Ok(value)
    }

    pub(crate) fn credential(&self) -> Result<&str, CliFailure> {
        std::str::from_utf8(&self.0).map_err(|_| authorization_denied())
    }

    fn normalize(&mut self) -> Result<(), CliFailure> {
        if self.0.ends_with(b"\r\n") {
            self.0.truncate(self.0.len() - 2);
        } else if self.0.ends_with(b"\n") {
            self.0.truncate(self.0.len() - 1);
        }
        if self.0.len() != MAX_API_KEY_CREDENTIAL_BYTES
            || self.0.iter().any(|byte| matches!(byte, b'\r' | b'\n' | 0))
        {
            return Err(authorization_denied());
        }
        Ok(())
    }
}

impl Drop for ApiKeyBuffer {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// One direct embedded client. It never opens a transport listener.
pub(crate) struct EmbeddedClient {
    product: NativeProduct,
    session: ProductSession,
    managed: bool,
    next_request_id: u128,
}

/// Exclusive credential-free client for OS-owner-authorized offline recovery.
pub(crate) struct OfflineOwnerClient {
    product: NativeProduct,
}

pub(crate) struct LegacyBearerBuffer(Vec<u8>);

impl LegacyBearerBuffer {
    pub(crate) fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for LegacyBearerBuffer {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

impl OfflineOwnerClient {
    pub(crate) fn open(data_dir: &Path) -> Result<Self, CliFailure> {
        NativeProduct::open_offline_owner(data_dir)
            .map(|product| Self { product })
            .map_err(Into::into)
    }

    pub(crate) fn inspect(&self) -> Result<OwnerRecoveryInspection, CliFailure> {
        self.product
            .inspect_owner_recovery_offline()
            .map_err(Into::into)
    }

    pub(crate) fn start(&mut self, label: &str) -> Result<OwnerRecoveryStartReceipt, CliFailure> {
        self.product
            .start_owner_recovery_offline(label, logical_time_micros())
            .map_err(Into::into)
    }

    pub(crate) fn resume(
        &mut self,
        pending_key_id: ApiKeyId,
        key_file: &Path,
        expected_epoch: AuthorizationEpoch,
    ) -> Result<OwnerRecoveryActivationReceipt, CliFailure> {
        let key = read_api_key_file(key_file)?;
        self.product
            .resume_owner_recovery_offline(
                pending_key_id,
                key.credential()?,
                expected_epoch,
                logical_time_micros(),
            )
            .map_err(Into::into)
    }

    pub(crate) fn abort(
        &mut self,
        pending_key_id: ApiKeyId,
        expected_epoch: AuthorizationEpoch,
    ) -> Result<OwnerRecoveryAbortReceipt, CliFailure> {
        self.product
            .abort_owner_recovery_offline(pending_key_id, expected_epoch, logical_time_micros())
            .map_err(Into::into)
    }

    pub(crate) fn start_legacy(
        &mut self,
        name: &str,
        label: &str,
        legacy_bearer: &LegacyBearerBuffer,
    ) -> Result<LegacyBearerMigrationStartReceipt, CliFailure> {
        self.product
            .start_legacy_bearer_migration_offline(
                name,
                label,
                legacy_bearer.expose(),
                logical_time_micros(),
            )
            .map_err(Into::into)
    }

    pub(crate) fn activate_legacy(
        &mut self,
        key_id: ApiKeyId,
        key_file: &Path,
        expected_epoch: AuthorizationEpoch,
        name: &str,
        label: &str,
        legacy_bearer: &LegacyBearerBuffer,
    ) -> Result<LegacyBearerMigrationActivationReceipt, CliFailure> {
        let key = read_api_key_file(key_file)?;
        self.product
            .activate_legacy_bearer_migration_offline(
                key_id,
                key.credential()?,
                expected_epoch,
                name,
                label,
                legacy_bearer.expose(),
                logical_time_micros(),
            )
            .map_err(Into::into)
    }
}

impl EmbeddedClient {
    pub(crate) fn open(
        product: NativeProduct,
        api_key_file: Option<&Path>,
        api_key_stdin: bool,
    ) -> Result<Self, CliFailure> {
        let credential = read_api_key(api_key_file, api_key_stdin)?;
        let status = product.access_control_status()?;
        let session_id = session_id()?;
        if status.bootstrapped {
            let credential = credential.ok_or_else(authorization_denied)?;
            let candidate = credential.credential()?;
            let authority = match product.authenticate_api_key(candidate, logical_time_micros()) {
                Ok(authority) => authority,
                Err(error) if error.code() == ProductErrorCode::AuthorizationDenied => {
                    product.authenticate_api_key_for_terminal_replay(candidate)?
                }
                Err(error) => return Err(error.into()),
            };
            return Ok(Self {
                product,
                session: ProductSession::new_authenticated(session_id, authority),
                managed: true,
                next_request_id: 1,
            });
        }
        if credential.is_some() {
            return Err(authorization_denied());
        }
        let principal = ProductPrincipal::new("local:cli").ok_or_else(CliFailure::internal)?;
        Ok(Self {
            product,
            session: ProductSession::new(session_id, principal, ProductAuthorization::ALL),
            managed: false,
            next_request_id: 1,
        })
    }

    pub(crate) fn dispatch(
        &mut self,
        operation: ProductOperation,
    ) -> Result<ProductResponse, Box<ProductError>> {
        self.dispatch_with_durability(operation, ProductDurability::Strict)
    }

    pub(crate) fn dispatch_with_durability(
        &mut self,
        operation: ProductOperation,
        durability: ProductDurability,
    ) -> Result<ProductResponse, Box<ProductError>> {
        self.dispatch_request(operation, durability, None, None)
    }

    pub(crate) fn dispatch_bounded(
        &mut self,
        operation: ProductOperation,
        cancellation: ProductCancellationToken,
        limits: ProductLimits,
    ) -> Result<ProductResponse, Box<ProductError>> {
        self.dispatch_request(
            operation,
            ProductDurability::Strict,
            None,
            Some((cancellation, limits)),
        )
    }

    pub(crate) fn dispatch_with_idempotency(
        &mut self,
        operation: ProductOperation,
        idempotency_token: u128,
    ) -> Result<ProductResponse, Box<ProductError>> {
        if idempotency_token == 0 {
            return Err(Box::new(ProductError::from_code(
                ProductErrorCode::InvalidRequest,
            )));
        }
        self.dispatch_request(
            operation,
            ProductDurability::Strict,
            Some(idempotency_token),
            None,
        )
    }

    fn dispatch_request(
        &mut self,
        operation: ProductOperation,
        durability: ProductDurability,
        idempotency_token: Option<u128>,
        bounded: Option<(ProductCancellationToken, ProductLimits)>,
    ) -> Result<ProductResponse, Box<ProductError>> {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.checked_add(1).ok_or_else(|| {
            Box::new(ProductError::from_code(
                hyphae_native_product::ProductErrorCode::LimitExceeded,
            ))
        })?;
        let mut context = hyphae_native_product::ProductRequestContext::new(
            request_id,
            self.session.id(),
            logical_time_micros(),
            self.session.principal().clone(),
            self.session.authorization(),
        )
        .with_authorization_epoch(self.session.authorization_epoch());
        if let Some(idempotency_token) = idempotency_token {
            context = context.with_idempotency_token(idempotency_token);
        }
        if let Some((cancellation, limits)) = bounded {
            context.cancellation = cancellation;
            context.limits = limits;
            let timeout_micros = i64::try_from(CONSOLE_REQUEST_TIMEOUT.as_micros())
                .map_err(|_| Box::new(ProductError::from_code(ProductErrorCode::Internal)))?;
            context.deadline_micros = Some(
                context
                    .logical_time_micros
                    .checked_add(timeout_micros)
                    .ok_or_else(|| Box::new(ProductError::from_code(ProductErrorCode::Internal)))?,
            );
        }
        context.durability.durability = durability;
        self.product
            .dispatch(&mut self.session, &context, operation)
            .map_err(Box::new)
    }

    pub(crate) fn capabilities(
        &mut self,
    ) -> Result<hyphae_native_product::ProductCapabilities, Box<ProductError>> {
        let ProductResponse::Capabilities(capabilities) =
            self.dispatch(ProductOperation::Capabilities)?
        else {
            return Err(Box::new(ProductError::from_code(
                ProductErrorCode::Internal,
            )));
        };
        Ok(capabilities)
    }

    pub(crate) const fn is_managed(&self) -> bool {
        self.managed
    }

    pub(crate) fn unmanaged_product_mut(
        &mut self,
    ) -> Result<&mut NativeProduct, Box<ProductError>> {
        if self.managed {
            return Err(Box::new(ProductError::from_code(
                ProductErrorCode::AuthorizationDenied,
            )));
        }
        Ok(&mut self.product)
    }
}

fn session_id() -> Result<ProductSessionId, CliFailure> {
    ProductSessionId::new(Uuid::now_v7().as_u128()).ok_or_else(CliFailure::internal)
}

pub(crate) fn authorization_denied() -> CliFailure {
    ProductError::from_code(ProductErrorCode::AuthorizationDenied).into()
}

fn read_api_key(
    api_key_file: Option<&Path>,
    api_key_stdin: bool,
) -> Result<Option<ApiKeyBuffer>, CliFailure> {
    match (api_key_file, api_key_stdin) {
        (None, false) => Ok(None),
        (Some(_), true) => Err(CliFailure::invalid()),
        (Some(path), false) => read_api_key_file(path).map(Some),
        (None, true) => read_bounded(io::stdin().lock()).map(Some),
    }
}

pub(crate) fn read_api_key_file(path: &Path) -> Result<ApiKeyBuffer, CliFailure> {
    #[cfg(windows)]
    if is_windows_named_stream(path) {
        return Err(authorization_denied());
    }
    let path_metadata = fs::symlink_metadata(path).map_err(|error| {
        #[cfg(windows)]
        {
            let _ = error;
            authorization_denied()
        }
        #[cfg(not(windows))]
        {
            CliFailure::from(error)
        }
    })?;
    if !path_metadata.file_type().is_file() || path_metadata.file_type().is_symlink() {
        return Err(authorization_denied());
    }
    #[cfg(not(windows))]
    let file = File::open(path)?;
    #[cfg(windows)]
    let file = (|| -> Result<File, CliFailure> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::{
            Foundation::GENERIC_READ,
            Storage::FileSystem::{
                FILE_FLAG_OPEN_REPARSE_POINT, READ_CONTROL, SECURITY_IDENTIFICATION,
            },
        };

        Ok(OpenOptions::new()
            .access_mode(GENERIC_READ | READ_CONTROL)
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .security_qos_flags(SECURITY_IDENTIFICATION)
            .open(path)?)
    })()
    .map_err(|_| authorization_denied())?;
    validate_open_api_key_file(path, &path_metadata, &file).map_err(|_| authorization_denied())?;
    read_bounded(file).map_err(|_| authorization_denied())
}

pub(crate) fn read_legacy_bearer_file(path: &Path) -> Result<LegacyBearerBuffer, CliFailure> {
    let metadata = fs::symlink_metadata(path).map_err(|_| authorization_denied())?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(authorization_denied());
    }
    let mut file = File::open(path).map_err(|_| authorization_denied())?;
    validate_open_api_key_file(path, &metadata, &file).map_err(|_| authorization_denied())?;
    let mut bytes = Vec::with_capacity(128);
    Read::by_ref(&mut file)
        .take(4_099)
        .read_to_end(&mut bytes)
        .map_err(|_| authorization_denied())?;
    if bytes.ends_with(b"\r\n") {
        bytes.truncate(bytes.len() - 2);
    } else if bytes.ends_with(b"\n") {
        bytes.truncate(bytes.len() - 1);
    }
    if !(32..=4_096).contains(&bytes.len())
        || bytes.iter().any(|byte| !(0x21..=0x7e).contains(byte))
        || bytes.starts_with(b"hyp1_")
    {
        bytes.fill(0);
        return Err(authorization_denied());
    }
    Ok(LegacyBearerBuffer(bytes))
}

pub(crate) struct RestrictedKeyOutputReservation {
    path: std::path::PathBuf,
    file: Option<File>,
    committed: bool,
}

impl RestrictedKeyOutputReservation {
    pub(crate) fn write_secret(&mut self, secret: &[u8]) -> Result<(), CliFailure> {
        let file = self.file.as_mut().ok_or_else(CliFailure::internal)?;
        if let Err(error) = file.write_all(secret).and_then(|()| file.sync_all()) {
            return Err(error.into());
        }
        sync_parent_io(&self.path)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for RestrictedKeyOutputReservation {
    fn drop(&mut self) {
        if !self.committed {
            self.file.take();
            let _ignored = fs::remove_file(&self.path);
            let _ignored = sync_parent_io(&self.path);
        }
    }
}

pub(crate) fn reserve_restricted_api_key_file(
    path: &Path,
) -> Result<RestrictedKeyOutputReservation, CliFailure> {
    #[cfg(windows)]
    if is_windows_named_stream(path) {
        return Err(CliFailure::invalid());
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::{Foundation::GENERIC_WRITE, Storage::FileSystem::READ_CONTROL};
        options
            .access_mode(GENERIC_WRITE | READ_CONTROL)
            .share_mode(0);
    }
    let file = options.open(path)?;
    #[cfg(windows)]
    if hyphae_native_product::restrict_windows_credential_file(path, &file).is_err() {
        drop(file);
        let _ignored = fs::remove_file(path);
        return Err(CliFailure::invalid());
    }
    if let Err(error) = file.sync_all().and_then(|()| sync_parent_io(path)) {
        drop(file);
        let _ignored = fs::remove_file(path);
        return Err(error.into());
    }
    Ok(RestrictedKeyOutputReservation {
        path: path.to_path_buf(),
        file: Some(file),
        committed: false,
    })
}

pub(crate) fn ensure_key_output_outside_data_dir(
    data_dir: &Path,
    output: &Path,
) -> Result<(), CliFailure> {
    if output.try_exists()? {
        return Err(CliFailure::from(ProductError::from_code(
            ProductErrorCode::Io,
        )));
    }
    let data = same_file::Handle::from_path(data_dir).map_err(|_| CliFailure::invalid())?;
    let canonical_data = fs::canonicalize(data_dir).map_err(|_| CliFailure::invalid())?;
    let output_parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut ancestor = Some(output_parent);
    while let Some(path) = ancestor {
        if path.try_exists()? {
            let candidate =
                same_file::Handle::from_path(path).map_err(|_| CliFailure::invalid())?;
            let canonical_candidate = fs::canonicalize(path).map_err(|_| CliFailure::invalid())?;
            if candidate == data || canonical_candidate.starts_with(&canonical_data) {
                return Err(CliFailure::invalid());
            }
        }
        ancestor = path.parent();
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent_io(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_io(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(windows)]
fn is_windows_named_stream(path: &Path) -> bool {
    use std::path::{Component, Prefix};

    path.components().any(|component| match component {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(_) | Prefix::VerbatimDisk(_) => false,
            _ => prefix.as_os_str().to_string_lossy().contains(':'),
        },
        Component::Normal(value) => value.to_string_lossy().contains(':'),
        Component::RootDir | Component::CurDir | Component::ParentDir => false,
    })
}

fn read_bounded(reader: impl Read) -> Result<ApiKeyBuffer, CliFailure> {
    let maximum_input =
        u64::try_from(MAX_API_KEY_CREDENTIAL_BYTES + 3).map_err(|_| CliFailure::internal())?;
    let mut bytes = Vec::with_capacity(MAX_API_KEY_CREDENTIAL_BYTES + 2);
    reader.take(maximum_input).read_to_end(&mut bytes)?;
    ApiKeyBuffer::from_bytes(bytes)
}

fn validate_open_api_key_file(
    path: &Path,
    path_metadata: &fs::Metadata,
    file: &File,
) -> Result<(), CliFailure> {
    let opened_metadata = file.metadata()?;
    let current_path_metadata = fs::symlink_metadata(path)?;
    if !opened_metadata.file_type().is_file()
        || !current_path_metadata.file_type().is_file()
        || current_path_metadata.file_type().is_symlink()
    {
        return Err(authorization_denied());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let identity = (opened_metadata.dev(), opened_metadata.ino());
        if identity != (path_metadata.dev(), path_metadata.ino())
            || identity != (current_path_metadata.dev(), current_path_metadata.ino())
            || opened_metadata.permissions().mode() & 0o077 != 0
        {
            return Err(authorization_denied());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

        let _ = path_metadata;
        if opened_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || hyphae_native_product::validate_windows_restricted_file(file).is_err()
        {
            return Err(authorization_denied());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io::Cursor};

    use super::*;

    #[test]
    fn credential_reader_accepts_one_terminal_newline_and_rejects_extra_bytes()
    -> Result<(), Box<dyn Error>> {
        let canonical = vec![b'x'; MAX_API_KEY_CREDENTIAL_BYTES];
        for suffix in [&b""[..], &b"\n"[..], &b"\r\n"[..]] {
            let mut input = canonical.clone();
            input.extend_from_slice(suffix);
            assert_eq!(
                read_bounded(Cursor::new(input))?.0.len(),
                MAX_API_KEY_CREDENTIAL_BYTES
            );
        }
        for mut invalid in [
            vec![b'x'; MAX_API_KEY_CREDENTIAL_BYTES + 1],
            vec![b'x'; MAX_API_KEY_CREDENTIAL_BYTES - 1],
        ] {
            invalid.push(b'\n');
            let error = read_bounded(Cursor::new(invalid))
                .err()
                .ok_or("invalid credential input was accepted")?;
            assert_eq!(error.error().code(), ProductErrorCode::AuthorizationDenied);
        }
        let mut nul = canonical;
        nul[7] = 0;
        let error = read_bounded(Cursor::new(nul))
            .err()
            .ok_or("NUL credential input was accepted")?;
        assert_eq!(error.error().code(), ProductErrorCode::AuthorizationDenied);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn credential_file_rejects_a_handle_with_a_different_identity() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir().join(format!("hyphae-key-race-{}", Uuid::now_v7()));
        fs::create_dir(&directory)?;
        let first = directory.join("first.key");
        let second = directory.join("second.key");
        fs::write(&first, vec![b'a'; MAX_API_KEY_CREDENTIAL_BYTES])?;
        fs::write(&second, vec![b'b'; MAX_API_KEY_CREDENTIAL_BYTES])?;
        fs::set_permissions(&first, fs::Permissions::from_mode(0o600))?;
        fs::set_permissions(&second, fs::Permissions::from_mode(0o600))?;
        let first_metadata = fs::symlink_metadata(&first)?;
        let second_handle = File::open(&second)?;
        let error = validate_open_api_key_file(&first, &first_metadata, &second_handle)
            .err()
            .ok_or("mismatched key-file handle was accepted")?;
        assert_eq!(error.error().code(), ProductErrorCode::AuthorizationDenied);
        fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn credential_file_rejects_a_reparse_point() -> Result<(), Box<dyn Error>> {
        use std::os::windows::fs::symlink_file;

        let directory = std::env::temp_dir().join(format!("hyphae-key-link-{}", Uuid::now_v7()));
        fs::create_dir(&directory)?;
        let target = directory.join("target.key");
        let link = directory.join("link.key");
        fs::write(&target, vec![b'a'; MAX_API_KEY_CREDENTIAL_BYTES])?;
        if symlink_file(&target, &link).is_err() {
            fs::remove_dir_all(directory)?;
            return Ok(());
        }
        let error = read_api_key_file(&link)
            .err()
            .ok_or("reparse-point key file was accepted")?;
        assert_eq!(error.error().code(), ProductErrorCode::AuthorizationDenied);
        fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn credential_file_rejects_a_named_stream() -> Result<(), Box<dyn Error>> {
        let directory = std::env::temp_dir().join(format!("hyphae-key-ads-{}", Uuid::now_v7()));
        fs::create_dir(&directory)?;
        let carrier = directory.join("carrier");
        let stream = std::path::PathBuf::from(format!("{}:owner.key", carrier.display()));
        fs::write(&carrier, b"carrier")?;
        fs::write(&stream, vec![b'a'; MAX_API_KEY_CREDENTIAL_BYTES])?;
        let error = read_api_key_file(&stream)
            .err()
            .ok_or("named-stream key file was accepted")?;
        assert_eq!(error.error().code(), ProductErrorCode::AuthorizationDenied);
        fs::remove_dir_all(directory)?;
        Ok(())
    }
}
