//! Preferences sync key bootstrap orchestration regression tests.
//!
//! @author kongweiguang

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use futures::executor::block_on;
use kerminal_application::{
    AppError, AppResult, PublicErrorCode,
    connections::credentials::{
        VaultKeyAvailability, VaultKeyDocument, VaultKeyExportMode, VaultKeyManagementService,
        VaultKeyOperationReceipt, VaultKeyRepository, VaultRecoveryReason,
    },
};

use super::read_vault_key_document;

#[derive(Clone)]
struct KeyRepositoryFixture {
    state: Arc<Mutex<KeyRepositoryState>>,
}

struct KeyRepositoryState {
    availability: VaultKeyAvailability,
    ensure_result: VaultKeyAvailability,
    document: Option<String>,
    read_error: Option<AppError>,
    availability_calls: usize,
    ensure_calls: usize,
    read_calls: usize,
}

impl KeyRepositoryFixture {
    /// Keep readiness, document contents, and operation counts isolated per regression case.
    fn new(
        availability: VaultKeyAvailability,
        ensure_result: VaultKeyAvailability,
        document: Option<String>,
        read_error: Option<AppError>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(KeyRepositoryState {
                availability,
                ensure_result,
                document,
                read_error,
                availability_calls: 0,
                ensure_calls: 0,
                read_calls: 0,
            })),
        }
    }

    /// Expose the fixture's observable calls so tests assert orchestration, not copied branch logic.
    fn calls(&self) -> (usize, usize, usize) {
        let state = self
            .state
            .lock()
            .expect("key repository fixture is not poisoned");
        (
            state.availability_calls,
            state.ensure_calls,
            state.read_calls,
        )
    }

    /// Read back the fixture state to prove a recovery condition was not replaced by bootstrap.
    fn availability_snapshot(&self) -> VaultKeyAvailability {
        self.state
            .lock()
            .expect("key repository fixture is not poisoned")
            .availability
            .clone()
    }

    /// Exercise the real application use case while keeping repository effects inspectable.
    fn service(&self) -> VaultKeyManagementService {
        VaultKeyManagementService::new(Arc::new(self.clone()))
    }
}

#[async_trait]
impl VaultKeyRepository for KeyRepositoryFixture {
    /// Return the configured state through the same application port used by the settings panel.
    async fn availability(&self) -> AppResult<VaultKeyAvailability> {
        let mut state = self
            .state
            .lock()
            .expect("key repository fixture is not poisoned");
        state.availability_calls += 1;
        Ok(state.availability.clone())
    }

    /// Model ensure as a visible state mutation so accidental recovery-state bootstrap fails tests.
    async fn ensure_key(&self) -> AppResult<VaultKeyAvailability> {
        let mut state = self
            .state
            .lock()
            .expect("key repository fixture is not poisoned");
        state.ensure_calls += 1;
        state.availability = state.ensure_result.clone();
        Ok(state.availability.clone())
    }

    /// Key creation is outside the document-read workflow exercised by these tests.
    async fn create_key(&self) -> AppResult<VaultKeyOperationReceipt> {
        panic!("settings document loading must not call explicit key creation")
    }

    /// Return the test document or preserve its configured read failure unchanged.
    async fn read_key(&self) -> AppResult<VaultKeyDocument> {
        let mut state = self
            .state
            .lock()
            .expect("key repository fixture is not poisoned");
        state.read_calls += 1;
        if let Some(error) = state.read_error.clone() {
            return Err(error);
        }
        state
            .document
            .clone()
            .map(VaultKeyDocument::new)
            .ok_or_else(|| AppError::not_found("测试 key 文档缺失"))
    }

    /// Export is not part of loading and must not be requested by the settings panel.
    async fn export_key(&self) -> AppResult<VaultKeyDocument> {
        panic!("settings document loading must not export a key")
    }

    /// Path export is not part of loading and must not access user-selected files.
    async fn export_key_to_path(
        &self,
        _path: PathBuf,
        _mode: VaultKeyExportMode,
    ) -> AppResult<VaultKeyOperationReceipt> {
        panic!("settings document loading must not export to a path")
    }

    /// Key saving is not part of loading and must not mutate the configured fixture document.
    async fn save_key(&self, _document: VaultKeyDocument) -> AppResult<VaultKeyOperationReceipt> {
        panic!("settings document loading must not save a key")
    }

    /// Import is not part of loading and must not replace the configured fixture document.
    async fn import_key(
        &self,
        _document: VaultKeyDocument,
        _dry_run: bool,
    ) -> AppResult<VaultKeyOperationReceipt> {
        panic!("settings document loading must not import a key")
    }

    /// File import is not part of loading and must not read an external path.
    async fn import_key_from_path(
        &self,
        _path: PathBuf,
        _dry_run: bool,
    ) -> AppResult<VaultKeyOperationReceipt> {
        panic!("settings document loading must not import from a path")
    }

    /// Pair recovery is deliberately explicit and must not run during a read-only page load.
    async fn restore_pair_from_paths(
        &self,
        _key_path: PathBuf,
        _vault_path: PathBuf,
        _dry_run: bool,
    ) -> AppResult<VaultKeyOperationReceipt> {
        panic!("settings document loading must not restore a key pair")
    }

    /// Rotation is not part of loading and must not rewrite key or vault state.
    async fn rotate_key(&self, _dry_run: bool) -> AppResult<VaultKeyOperationReceipt> {
        panic!("settings document loading must not rotate a key")
    }
}

/// A fresh workspace initializes once, then presents the key document returned by the use case.
#[test]
fn fresh_workspace_bootstraps_key_before_reading_document() {
    let repository = KeyRepositoryFixture::new(
        VaultKeyAvailability::NotInitialized,
        VaultKeyAvailability::Created {
            key_id: "fresh-key".to_owned(),
        },
        Some("schema_version = 1\nkey_id = \"fresh-key\"".to_owned()),
        None,
    );

    let (availability, document) = block_on(read_vault_key_document(repository.service()));

    assert_eq!(
        availability,
        Some(VaultKeyAvailability::Created {
            key_id: "fresh-key".to_owned(),
        })
    );
    assert_eq!(
        document
            .expect("fresh key document is readable")
            .expect("fresh workspace exposes its initialized key document")
            .as_str(),
        "schema_version = 1\nkey_id = \"fresh-key\""
    );
    assert_eq!(repository.calls(), (1, 1, 1));
}

/// An existing encrypted vault without its key remains a recovery condition and is never bootstrapped.
#[test]
fn existing_vault_without_key_is_not_replaced_by_bootstrap() {
    let repository = KeyRepositoryFixture::new(
        VaultKeyAvailability::MissingForExistingVault,
        VaultKeyAvailability::Created {
            key_id: "would-overwrite-existing-vault".to_owned(),
        },
        None,
        Some(AppError::not_found("测试 vault key 缺失")),
    );

    let (availability, document) = block_on(read_vault_key_document(repository.service()));

    assert_eq!(
        availability,
        Some(VaultKeyAvailability::MissingForExistingVault)
    );
    assert_eq!(
        document
            .expect_err("missing key must remain visible")
            .public_code(),
        PublicErrorCode::NotFound
    );
    assert_eq!(
        repository.availability_snapshot(),
        VaultKeyAvailability::MissingForExistingVault
    );
    assert_eq!(repository.calls(), (1, 0, 1));
}

/// A malformed key keeps its original read failure and recovery status instead of receiving a new key.
#[test]
fn damaged_key_read_error_is_preserved_without_bootstrap() {
    let recovery = VaultKeyAvailability::RecoveryRequired {
        reason: VaultRecoveryReason::InvalidKey,
    };
    let repository = KeyRepositoryFixture::new(
        recovery.clone(),
        VaultKeyAvailability::Created {
            key_id: "would-hide-damaged-key".to_owned(),
        },
        None,
        Some(AppError::invalid("测试 key 文档损坏")),
    );

    let (availability, document) = block_on(read_vault_key_document(repository.service()));

    assert_eq!(availability, Some(recovery.clone()));
    assert_eq!(
        document
            .expect_err("damaged key read error must remain visible")
            .public_code(),
        PublicErrorCode::Invalid
    );
    assert_eq!(repository.availability_snapshot(), recovery);
    assert_eq!(repository.calls(), (1, 0, 1));
}
