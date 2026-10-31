//! In-memory device pairing offers (M8 E9-1).
//!
//! Flow:
//! 1. The signed-in desktop creates an offer: a one-time QR token plus a
//!    6-digit code that is shown on the desktop only (never in the QR).
//! 2. The phone scans the QR and calls `start` with the token, its name and
//!    platform. The QR token is consumed; a second `start` with it fails.
//!    The desktop is notified ("device X requests pairing") and may reject.
//! 3. The phone calls `confirm` with the code. Five wrong codes void the
//!    pairing. On success the caller issues the device token and reports the
//!    new device id back with [`DevicePairingStore::complete`].
//!
//! Offers are separate from [`crate::QrTokenStore`] on purpose: a pairing QR
//! must not be redeemable at `/api/auth/qr-login` for a full session JWT.
use std::fmt::Write as _;
use std::sync::{Arc, OnceLock};

use dashmap::DashMap;
use serde::Serialize;

/// Window for the phone to scan the QR (and, separately, to enter the code
/// after scanning).
pub const PAIRING_TTL_MS: i64 = 5 * 60 * 1000;
/// Wrong codes allowed before a pairing is voided.
pub const MAX_CODE_ATTEMPTS: u32 = 5;
/// WebSocket event announcing an offer status change to its desktop user.
pub const DEVICE_PAIRING_EVENT: &str = "device.pairing";

const QR_TOKEN_BYTES: usize = 32;
const ID_BYTES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OfferStatus {
    /// QR shown, not scanned yet.
    Waiting,
    /// Phone scanned; waiting for the code.
    Requested,
    /// Code accepted; device token being issued.
    Confirming,
    Paired,
    /// The desktop user rejected or cancelled it.
    Rejected,
    /// Too many wrong codes, or issuing the device failed.
    Voided,
    Expired,
}

impl OfferStatus {
    fn is_open(self) -> bool {
        matches!(self, Self::Waiting | Self::Requested)
    }
}

/// Returned to the desktop when it creates an offer.
#[derive(Debug, Clone, Serialize)]
pub struct PairingOffer {
    pub offer_id: String,
    /// Encoded in the QR.
    pub qr_token: String,
    /// Shown on the desktop only.
    pub code: String,
    pub expires_at: i64,
}

/// Desktop-facing view of an offer (and the WebSocket event payload).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PairingOfferView {
    pub user_id: String,
    pub offer_id: String,
    pub status: OfferStatus,
    pub device_name: Option<String>,
    pub platform: Option<String>,
    pub device_id: Option<String>,
    pub expires_at: i64,
}

/// Returned to the phone by `start`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedPairing {
    pub pairing_id: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingStartError {
    /// Unknown, expired, cancelled, or already-used QR token.
    InvalidQr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingConfirmError {
    /// Wrong code; the pairing is still open.
    WrongCode { attempts_left: u32 },
    /// Unknown, expired, rejected, voided or already-used pairing.
    Closed { status: Option<OfferStatus> },
}

/// Accepted code: who to issue the device for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedPairing {
    pub offer_id: String,
    pub user_id: String,
    pub device_name: String,
    pub platform: String,
}

/// Callback invoked on every offer status change.
pub type PairingNotifier = Arc<dyn Fn(&PairingOfferView) + Send + Sync>;

struct Offer {
    user_id: String,
    qr_token: String,
    code: String,
    created_at: i64,
    status: OfferStatus,
    pairing_id: Option<String>,
    started_at: Option<i64>,
    device_name: Option<String>,
    platform: Option<String>,
    failed_attempts: u32,
    device_id: Option<String>,
}

impl Offer {
    fn expires_at(&self) -> i64 {
        self.started_at.unwrap_or(self.created_at) + PAIRING_TTL_MS
    }

    /// Status with expiry applied lazily.
    fn effective_status(&self, now: i64) -> OfferStatus {
        if self.status.is_open() && now >= self.expires_at() {
            OfferStatus::Expired
        } else {
            self.status
        }
    }

    fn view(&self, offer_id: &str, now: i64) -> PairingOfferView {
        PairingOfferView {
            user_id: self.user_id.clone(),
            offer_id: offer_id.to_owned(),
            status: self.effective_status(now),
            device_name: self.device_name.clone(),
            platform: self.platform.clone(),
            device_id: self.device_id.clone(),
            expires_at: self.expires_at(),
        }
    }
}

/// Offers keyed by id, with QR-token and pairing-id indexes.
#[derive(Default)]
pub struct DevicePairingStore {
    offers: DashMap<String, Offer>,
    by_qr: DashMap<String, String>,
    by_pairing: DashMap<String, String>,
    notifier: OnceLock<PairingNotifier>,
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::getrandom(&mut buf).expect("OS entropy source unavailable");
    let mut out = String::with_capacity(bytes * 2);
    for byte in buf {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Uniform 6-digit code (rejection sampling avoids modulo bias).
fn random_code() -> String {
    const LIMIT: u32 = 4_294_000_000; // largest multiple of 1_000_000 below u32::MAX
    loop {
        let mut buf = [0u8; 4];
        getrandom::getrandom(&mut buf).expect("OS entropy source unavailable");
        let value = u32::from_le_bytes(buf);
        if value < LIMIT {
            return format!("{:06}", value % 1_000_000);
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl DevicePairingStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_notifier(&self, notifier: PairingNotifier) {
        let _ = self.notifier.set(notifier);
    }

    fn notify(&self, view: &PairingOfferView) {
        if let Some(notifier) = self.notifier.get() {
            notifier(view);
        }
    }

    /// Drop offers that closed (or expired) more than one TTL ago.
    fn purge(&self, now: i64) {
        let stale: Vec<String> = self
            .offers
            .iter()
            .filter(|entry| now >= entry.value().expires_at() + PAIRING_TTL_MS)
            .map(|entry| entry.key().clone())
            .collect();
        for offer_id in stale {
            if let Some((_, offer)) = self.offers.remove(&offer_id) {
                self.by_qr.remove(&offer.qr_token);
                if let Some(pairing_id) = offer.pairing_id {
                    self.by_pairing.remove(&pairing_id);
                }
            }
        }
    }

    /// Mint an offer for the signed-in desktop user.
    pub fn create_offer(&self, user_id: &str) -> PairingOffer {
        self.create_offer_at(user_id, aionui_common::now_ms())
    }

    pub(crate) fn create_offer_at(&self, user_id: &str, now: i64) -> PairingOffer {
        self.purge(now);
        let offer_id = format!("pair_{}", random_hex(ID_BYTES));
        let qr_token = random_hex(QR_TOKEN_BYTES);
        let code = random_code();
        let offer = Offer {
            user_id: user_id.to_owned(),
            qr_token: qr_token.clone(),
            code: code.clone(),
            created_at: now,
            status: OfferStatus::Waiting,
            pairing_id: None,
            started_at: None,
            device_name: None,
            platform: None,
            failed_attempts: 0,
            device_id: None,
        };
        let expires_at = offer.expires_at();
        self.offers.insert(offer_id.clone(), offer);
        self.by_qr.insert(qr_token.clone(), offer_id.clone());
        PairingOffer {
            offer_id,
            qr_token,
            code,
            expires_at,
        }
    }

    /// Desktop view of its own offer.
    pub fn view(&self, user_id: &str, offer_id: &str) -> Option<PairingOfferView> {
        let now = aionui_common::now_ms();
        self.offers
            .get(offer_id)
            .filter(|offer| offer.user_id == user_id)
            .map(|offer| offer.view(offer_id, now))
    }

    /// Phone scanned the QR. Consumes the QR token.
    pub fn start(
        &self,
        qr_token: &str,
        device_name: &str,
        platform: &str,
    ) -> Result<StartedPairing, PairingStartError> {
        self.start_at(qr_token, device_name, platform, aionui_common::now_ms())
    }

    pub(crate) fn start_at(
        &self,
        qr_token: &str,
        device_name: &str,
        platform: &str,
        now: i64,
    ) -> Result<StartedPairing, PairingStartError> {
        // Removing the index entry is the single-use gate.
        let (_, offer_id) = self.by_qr.remove(qr_token).ok_or(PairingStartError::InvalidQr)?;
        let (started, view) = {
            let mut offer = self.offers.get_mut(&offer_id).ok_or(PairingStartError::InvalidQr)?;
            if offer.effective_status(now) != OfferStatus::Waiting {
                return Err(PairingStartError::InvalidQr);
            }
            let pairing_id = format!("pairing_{}", random_hex(ID_BYTES));
            offer.status = OfferStatus::Requested;
            offer.pairing_id = Some(pairing_id.clone());
            offer.started_at = Some(now);
            offer.device_name = Some(device_name.to_owned());
            offer.platform = Some(platform.to_owned());
            self.by_pairing.insert(pairing_id.clone(), offer_id.clone());
            (
                StartedPairing {
                    pairing_id,
                    expires_at: offer.expires_at(),
                },
                offer.view(&offer_id, now),
            )
        };
        self.notify(&view);
        Ok(started)
    }

    /// Phone entered the code.
    pub fn confirm(&self, pairing_id: &str, code: &str) -> Result<ConfirmedPairing, PairingConfirmError> {
        self.confirm_at(pairing_id, code, aionui_common::now_ms())
    }

    pub(crate) fn confirm_at(
        &self,
        pairing_id: &str,
        code: &str,
        now: i64,
    ) -> Result<ConfirmedPairing, PairingConfirmError> {
        let offer_id = self
            .by_pairing
            .get(pairing_id)
            .map(|entry| entry.value().clone())
            .ok_or(PairingConfirmError::Closed { status: None })?;
        let (result, view) = {
            let mut offer = self
                .offers
                .get_mut(&offer_id)
                .ok_or(PairingConfirmError::Closed { status: None })?;
            let status = offer.effective_status(now);
            if status != OfferStatus::Requested {
                return Err(PairingConfirmError::Closed { status: Some(status) });
            }
            if constant_time_eq(offer.code.as_bytes(), code.trim().as_bytes()) {
                offer.status = OfferStatus::Confirming;
                let confirmed = ConfirmedPairing {
                    offer_id: offer_id.clone(),
                    user_id: offer.user_id.clone(),
                    device_name: offer.device_name.clone().unwrap_or_default(),
                    platform: offer.platform.clone().unwrap_or_default(),
                };
                (Ok(confirmed), None)
            } else {
                offer.failed_attempts += 1;
                if offer.failed_attempts >= MAX_CODE_ATTEMPTS {
                    offer.status = OfferStatus::Voided;
                    (
                        Err(PairingConfirmError::Closed {
                            status: Some(OfferStatus::Voided),
                        }),
                        Some(offer.view(&offer_id, now)),
                    )
                } else {
                    (
                        Err(PairingConfirmError::WrongCode {
                            attempts_left: MAX_CODE_ATTEMPTS - offer.failed_attempts,
                        }),
                        None,
                    )
                }
            }
        };
        if let Some(view) = view {
            self.notify(&view);
        }
        result
    }

    /// Record the outcome of issuing the device after a confirmed code:
    /// `Some(device_id)` marks the offer paired, `None` voids it.
    pub fn complete(&self, offer_id: &str, device_id: Option<&str>) {
        let now = aionui_common::now_ms();
        let view = {
            let Some(mut offer) = self.offers.get_mut(offer_id) else {
                return;
            };
            if offer.status != OfferStatus::Confirming {
                return;
            }
            match device_id {
                Some(id) => {
                    offer.status = OfferStatus::Paired;
                    offer.device_id = Some(id.to_owned());
                }
                None => offer.status = OfferStatus::Voided,
            }
            offer.view(offer_id, now)
        };
        self.notify(&view);
    }

    /// Desktop rejects (after a scan) or cancels (before one) its own offer.
    /// Returns the updated view, or `None` if it is not the user's open offer.
    pub fn reject(&self, user_id: &str, offer_id: &str) -> Option<PairingOfferView> {
        let now = aionui_common::now_ms();
        let view = {
            let mut offer = self.offers.get_mut(offer_id).filter(|offer| offer.user_id == user_id)?;
            if !offer.effective_status(now).is_open() {
                return None;
            }
            offer.status = OfferStatus::Rejected;
            self.by_qr.remove(&offer.qr_token);
            offer.view(offer_id, now)
        };
        self.notify(&view);
        Some(view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn code_is_six_digits_and_not_in_the_qr() {
        let store = DevicePairingStore::new();
        let t = aionui_common::now_ms();
        let offer = store.create_offer_at("u1", t);
        assert_eq!(offer.code.len(), 6);
        assert!(offer.code.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(offer.qr_token.len(), 64);
        assert_eq!(offer.expires_at, t + PAIRING_TTL_MS);
        let view = store.view("u1", &offer.offer_id).unwrap();
        assert_eq!(view.status, OfferStatus::Waiting);
    }

    #[test]
    fn happy_path_and_qr_is_single_use() {
        let store = DevicePairingStore::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        store.set_notifier(Arc::new(move |view| sink.lock().unwrap().push(view.status)));

        let t = aionui_common::now_ms();
        let offer = store.create_offer_at("u1", t);
        let started = store.start_at(&offer.qr_token, "Pixel", "android", t + 10).unwrap();
        assert_eq!(
            store.start_at(&offer.qr_token, "Evil", "ios", t + 11),
            Err(PairingStartError::InvalidQr)
        );
        let view = store.view("u1", &offer.offer_id).unwrap();
        assert_eq!(view.status, OfferStatus::Requested);
        assert_eq!(view.device_name.as_deref(), Some("Pixel"));
        assert!(store.view("u2", &offer.offer_id).is_none());

        let confirmed = store.confirm_at(&started.pairing_id, &offer.code, t + 20).unwrap();
        assert_eq!(confirmed.user_id, "u1");
        assert_eq!(confirmed.platform, "android");
        // The same code cannot mint a second device.
        assert!(matches!(
            store.confirm_at(&started.pairing_id, &offer.code, t + 21),
            Err(PairingConfirmError::Closed { .. })
        ));
        store.complete(&offer.offer_id, Some("dev_1"));
        let view = store.view("u1", &offer.offer_id).unwrap();
        assert_eq!(view.status, OfferStatus::Paired);
        assert_eq!(view.device_id.as_deref(), Some("dev_1"));
        assert_eq!(
            *events.lock().unwrap(),
            vec![OfferStatus::Requested, OfferStatus::Paired]
        );
    }

    #[test]
    fn five_wrong_codes_void_the_pairing() {
        let store = DevicePairingStore::new();
        let offer = store.create_offer_at("u1", 0);
        let started = store.start_at(&offer.qr_token, "Pixel", "android", 0).unwrap();
        let wrong = if offer.code == "000000" { "111111" } else { "000000" };
        for left in (1..MAX_CODE_ATTEMPTS).rev() {
            assert_eq!(
                store.confirm_at(&started.pairing_id, wrong, 1),
                Err(PairingConfirmError::WrongCode { attempts_left: left })
            );
        }
        assert_eq!(
            store.confirm_at(&started.pairing_id, wrong, 1),
            Err(PairingConfirmError::Closed {
                status: Some(OfferStatus::Voided)
            })
        );
        // Even the right code is refused now.
        assert_eq!(
            store.confirm_at(&started.pairing_id, &offer.code, 1),
            Err(PairingConfirmError::Closed {
                status: Some(OfferStatus::Voided)
            })
        );
    }

    #[test]
    fn expiry_and_rejection_close_the_offer() {
        let store = DevicePairingStore::new();
        let offer = store.create_offer_at("u1", 0);
        assert_eq!(
            store.start_at(&offer.qr_token, "Pixel", "android", PAIRING_TTL_MS),
            Err(PairingStartError::InvalidQr)
        );

        let offer = store.create_offer_at("u1", 0);
        let started = store.start_at(&offer.qr_token, "Pixel", "android", 100).unwrap();
        assert_eq!(
            store.confirm_at(&started.pairing_id, &offer.code, 100 + PAIRING_TTL_MS),
            Err(PairingConfirmError::Closed {
                status: Some(OfferStatus::Expired)
            })
        );

        let offer = store.create_offer_at("u1", aionui_common::now_ms());
        let started = store
            .start_at(&offer.qr_token, "Pixel", "android", aionui_common::now_ms())
            .unwrap();
        assert!(store.reject("u2", &offer.offer_id).is_none());
        assert_eq!(
            store.reject("u1", &offer.offer_id).unwrap().status,
            OfferStatus::Rejected
        );
        assert!(matches!(
            store.confirm(&started.pairing_id, &offer.code),
            Err(PairingConfirmError::Closed {
                status: Some(OfferStatus::Rejected)
            })
        ));

        // Cancelling before a scan also kills the QR.
        let offer = store.create_offer_at("u1", aionui_common::now_ms());
        store.reject("u1", &offer.offer_id).unwrap();
        assert_eq!(
            store.start(&offer.qr_token, "Pixel", "android"),
            Err(PairingStartError::InvalidQr)
        );
    }

    #[test]
    fn failed_issuance_voids_the_offer() {
        let store = DevicePairingStore::new();
        let now = aionui_common::now_ms();
        let offer = store.create_offer_at("u1", now);
        let started = store.start_at(&offer.qr_token, "Pixel", "ios", now).unwrap();
        store.confirm_at(&started.pairing_id, &offer.code, now).unwrap();
        store.complete(&offer.offer_id, None);
        assert_eq!(store.view("u1", &offer.offer_id).unwrap().status, OfferStatus::Voided);
    }
}
