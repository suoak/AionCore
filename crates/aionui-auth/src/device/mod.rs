//! Paired mobile devices (M8 E9-1).
//!
//! - [`token`]: opaque per-device tokens (`wmd_` + 32 random bytes) and their
//!   SHA-256 hash, the only form persisted (`user_devices.token_hash`).
//! - [`pairing`]: in-memory pairing offers. The desktop mints an offer (QR
//!   token + 6-digit code shown only on the desktop); the phone starts the
//!   pairing with the QR token and confirms it with the code.
//! - [`scope`]: the `mobile_default` route whitelist enforced on requests
//!   authenticated with a device token.
//! - [`routes`]: `/api/auth/devices/pair/*` endpoints.
pub mod pairing;
pub mod routes;
pub mod scope;
pub mod token;

pub use pairing::{
    DEVICE_PAIRING_EVENT, DevicePairingStore, OfferStatus, PairingConfirmError, PairingNotifier, PairingOffer,
    PairingOfferView, PairingStartError, StartedPairing,
};
pub use routes::{DeviceRouterState, device_pairing_public_routes, device_pairing_user_routes};
pub use scope::{DeviceRouteDecision, classify_device_request};
pub use token::{
    DEVICE_INACTIVITY_TTL_MS, DEVICE_TOKEN_PREFIX, DevicePrincipal, MAX_DEVICES_PER_USER, MOBILE_DEFAULT_SCOPE,
    generate_device_token, hash_device_token, is_device_token,
};
