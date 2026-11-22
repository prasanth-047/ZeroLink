use jni::objects::{JByteArray, JClass, JString};
use jni::sys::{jboolean, jbyteArray, jint, jlong};
use jni::JNIEnv;
use std::sync::Mutex;
use zerolink_core::crypto::{KeyExchange, SessionCipher, SessionRole};
use zerolink_core::framing::{Frame, PacketType};
use zerolink_core::protocol::{ClipboardPayload, ClipboardType, NotificationPayload};

static SERVER_KX: Mutex<Option<KeyExchange>> = Mutex::new(None);
static SESSION_CIPHER: Mutex<Option<SessionCipher>> = Mutex::new(None);

#[no_mangle]
pub extern "system" fn Java_org_zerolink_jni_ZeroLinkNative_pairingCode(
    env: JNIEnv,
    _class: JClass,
    client_pub_bytes: JByteArray,
    salt_bytes: JByteArray,
    server_pub_bytes: JByteArray,
) -> jni::sys::jstring {
    let client = match env.convert_byte_array(&client_pub_bytes) {
        Ok(v) if v.len() == 32 => v,
        _ => return std::ptr::null_mut(),
    };
    let salt = match env.convert_byte_array(&salt_bytes) {
        Ok(v) if v.len() == 32 => v,
        _ => return std::ptr::null_mut(),
    };
    let server = match env.convert_byte_array(&server_pub_bytes) {
        Ok(v) if v.len() == 32 => v,
        _ => return std::ptr::null_mut(),
    };
    let client: [u8; 32] = client.try_into().expect("length checked");
    let salt: [u8; 32] = salt.try_into().expect("length checked");
    let server: [u8; 32] = server.try_into().expect("length checked");
    let code = zerolink_core::crypto::pairing_code(&client, &salt, &server);
    match env.new_string(code) {
        Ok(value) => value.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "system" fn Java_org_zerolink_jni_ZeroLinkNative_deriveWifiBulkKeys(
    env: JNIEnv,
    _class: JClass,
    session_token: JByteArray,
    server_challenge: JByteArray,
) -> jbyteArray {
    let token = match env.convert_byte_array(&session_token) {
        Ok(bytes) if bytes.len() == 32 => bytes,
        _ => return std::ptr::null_mut(),
    };
    let challenge = match env.convert_byte_array(&server_challenge) {
        Ok(bytes) if bytes.len() == 32 => bytes,
        _ => return std::ptr::null_mut(),
    };
    let token: [u8; 32] = token.try_into().expect("length checked");
    let challenge: [u8; 32] = challenge.try_into().expect("length checked");
    let keys = zerolink_core::crypto::derive_wifi_bulk_keys(&token, &challenge);
    match env.byte_array_from_slice(&keys) {
        Ok(array) => array.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "system" fn Java_org_zerolink_jni_ZeroLinkNative_generateServerEphemeralKey(
    env: JNIEnv,
    _class: JClass,
) -> jbyteArray {
    let kx = KeyExchange::new();
    let pub_key = kx.public_key;

    let mut lock = match SERVER_KX.lock() {
        Ok(l) => l,
        Err(poisoned) => poisoned.into_inner(),
    };
    *lock = Some(kx);

    match env.byte_array_from_slice(&pub_key) {
        Ok(arr) => arr.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "system" fn Java_org_zerolink_jni_ZeroLinkNative_completeHandshake(
    env: JNIEnv,
    _class: JClass,
    client_pub_bytes: JByteArray,
    salt_bytes: JByteArray,
) -> jboolean {
    let client_vec = match env.convert_byte_array(&client_pub_bytes) {
        Ok(v) => v,
        Err(_) => return 0,
    };
    let salt_vec = match env.convert_byte_array(&salt_bytes) {
        Ok(v) => v,
        Err(_) => return 0,
    };

    if client_vec.len() != 32 || salt_vec.len() != 32 {
        return 0;
    }

    let mut client_arr = [0u8; 32];
    let mut salt_arr = [0u8; 32];
    client_arr.copy_from_slice(&client_vec);
    salt_arr.copy_from_slice(&salt_vec);

    let mut kx_lock = match SERVER_KX.lock() {
        Ok(l) => l,
        Err(poisoned) => poisoned.into_inner(),
    };
    let kx = match kx_lock.take() {
        Some(k) => k,
        None => return 0,
    };

    match kx.complete_handshake(&client_arr, &salt_arr, SessionRole::Responder) {
        Ok(cipher) => {
            let mut cipher_lock = match SESSION_CIPHER.lock() {
                Ok(l) => l,
                Err(poisoned) => poisoned.into_inner(),
            };
            *cipher_lock = Some(cipher);
            1
        }
        Err(_) => 0,
    }
}

#[no_mangle]
pub extern "system" fn Java_org_zerolink_jni_ZeroLinkNative_keyConfirmationTag(
    env: JNIEnv,
    _class: JClass,
    transcript_bytes: JByteArray,
) -> jbyteArray {
    let transcript = match env.convert_byte_array(&transcript_bytes) {
        Ok(v) => v,
        Err(_) => return std::ptr::null_mut(),
    };
    let lock = match SESSION_CIPHER.lock() {
        Ok(l) => l,
        Err(poisoned) => poisoned.into_inner(),
    };
    let cipher = match lock.as_ref() {
        Some(c) => c,
        None => return std::ptr::null_mut(),
    };
    let tag = cipher.key_confirmation_tag(&transcript);
    match env.byte_array_from_slice(&tag) {
        Ok(arr) => arr.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "system" fn Java_org_zerolink_jni_ZeroLinkNative_pairingSecret(
    env: JNIEnv,
    _class: JClass,
) -> jbyteArray {
    let lock = match SESSION_CIPHER.lock() {
        Ok(l) => l,
        Err(poisoned) => poisoned.into_inner(),
    };
    let secret = match lock.as_ref() {
        Some(cipher) => cipher.pairing_secret(),
        None => return std::ptr::null_mut(),
    };
    match env.byte_array_from_slice(&secret) {
        Ok(arr) => arr.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "system" fn Java_org_zerolink_jni_ZeroLinkNative_encryptAndFramePacket(
    env: JNIEnv,
    _class: JClass,
    packet_type_val: jint,
    plaintext_data: JByteArray,
) -> jbyteArray {
    let plain = match env.convert_byte_array(&plaintext_data) {
        Ok(v) => v,
        Err(_) => return std::ptr::null_mut(),
    };

    let ptype = match PacketType::from_u8(packet_type_val as u8) {
        Ok(pt) => pt,
        Err(_) => return std::ptr::null_mut(),
    };

    let mut cipher_lock = match SESSION_CIPHER.lock() {
        Ok(l) => l,
        Err(poisoned) => poisoned.into_inner(),
    };
    let cipher = match cipher_lock.as_mut() {
        Some(c) => c,
        None => return std::ptr::null_mut(),
    };

    let (seq, ciphertext) = match cipher.encrypt(&plain, &[ptype as u8]) {
        Ok(res) => res,
        Err(_) => return std::ptr::null_mut(),
    };

    let frame = match Frame::new(ptype, seq, ciphertext) {
        Ok(f) => f,
        Err(_) => return std::ptr::null_mut(),
    };

    let framed_bytes = frame.serialize();
    match env.byte_array_from_slice(&framed_bytes) {
        Ok(out) => out.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "system" fn Java_org_zerolink_jni_ZeroLinkNative_decryptFramedPacket(
    env: JNIEnv,
    _class: JClass,
    framed_data: JByteArray,
) -> jbyteArray {
    let raw = match env.convert_byte_array(&framed_data) {
        Ok(v) => v,
        Err(_) => return std::ptr::null_mut(),
    };

    let (frame, _) = match Frame::try_parse(&raw) {
        Ok(Some(res)) => res,
        _ => return std::ptr::null_mut(),
    };

    let mut cipher_lock = match SESSION_CIPHER.lock() {
        Ok(l) => l,
        Err(poisoned) => poisoned.into_inner(),
    };
    let cipher = match cipher_lock.as_mut() {
        Some(c) => c,
        None => return std::ptr::null_mut(),
    };

    let decrypted = match cipher.decrypt(
        frame.header.sequence,
        &frame.payload,
        &[frame.header.packet_type as u8],
    ) {
        Ok(d) => d,
        Err(_) => return std::ptr::null_mut(),
    };

    match env.byte_array_from_slice(&decrypted) {
        Ok(out) => out.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "system" fn Java_org_zerolink_jni_ZeroLinkNative_serializeNotification(
    mut env: JNIEnv,
    _class: JClass,
    id: JString,
    package_name: JString,
    app_name: JString,
    title: JString,
    content: JString,
    timestamp_ms: jlong,
    is_ongoing: jboolean,
) -> jbyteArray {
    let id_str: String = env
        .get_string(&id)
        .ok()
        .map(String::from)
        .unwrap_or_default();
    let pkg_str: String = env
        .get_string(&package_name)
        .ok()
        .map(String::from)
        .unwrap_or_default();
    let app_str: String = env
        .get_string(&app_name)
        .ok()
        .map(String::from)
        .unwrap_or_default();
    let title_str: String = env
        .get_string(&title)
        .ok()
        .map(String::from)
        .unwrap_or_default();
    let content_str: String = env
        .get_string(&content)
        .ok()
        .map(String::from)
        .unwrap_or_default();

    let notif = NotificationPayload {
        id: id_str,
        package_name: pkg_str,
        app_name: app_str,
        title: title_str,
        content: content_str,
        timestamp_ms: timestamp_ms as u64,
        is_ongoing: is_ongoing != 0,
    };

    let bytes = notif.serialize();
    match env.byte_array_from_slice(&bytes) {
        Ok(out) => out.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "system" fn Java_org_zerolink_jni_ZeroLinkNative_serializeClipboard(
    env: JNIEnv,
    _class: JClass,
    format_val: jint,
    payload_hash: jlong,
    timestamp_ms: jlong,
    data: JByteArray,
) -> jbyteArray {
    let data_vec = match env.convert_byte_array(&data) {
        Ok(v) => v,
        Err(_) => return std::ptr::null_mut(),
    };

    let format = match format_val {
        0 => ClipboardType::PlainText,
        1 => ClipboardType::HtmlText,
        2 => ClipboardType::PngImage,
        3 => ClipboardType::UriList,
        _ => ClipboardType::PlainText,
    };

    let clip = ClipboardPayload {
        format,
        payload_hash: payload_hash as u64,
        timestamp_ms: timestamp_ms as u64,
        data: data_vec,
    };

    let bytes = clip.serialize();
    match env.byte_array_from_slice(&bytes) {
        Ok(out) => out.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}
