use jni::objects::{JByteArray, JClass, JString};
use jni::sys::{jboolean, jbyteArray, jint, jlong};
use jni::JNIEnv;
use std::sync::Mutex;
use zerolink_core::crypto::{KeyExchange, SessionCipher};
use zerolink_core::framing::{Frame, PacketType};
use zerolink_core::protocol::{ClipboardPayload, ClipboardType, NotificationPayload};

static SERVER_KX: Mutex<Option<KeyExchange>> = Mutex::new(None);
static SESSION_CIPHER: Mutex<Option<SessionCipher>> = Mutex::new(None);

#[no_mangle]
pub extern "system" fn Java_org_zerolink_jni_ZeroLinkNative_generateServerEphemeralKey(
    mut env: JNIEnv,
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
    mut env: JNIEnv,
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

    match kx.complete_handshake(&client_arr, &salt_arr) {
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
pub extern "system" fn Java_org_zerolink_jni_ZeroLinkNative_encryptAndFramePacket(
    mut env: JNIEnv,
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
    mut env: JNIEnv,
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
        .map(|s| String::from(s))
        .unwrap_or_default();
    let pkg_str: String = env
        .get_string(&package_name)
        .ok()
        .map(|s| String::from(s))
        .unwrap_or_default();
    let app_str: String = env
        .get_string(&app_name)
        .ok()
        .map(|s| String::from(s))
        .unwrap_or_default();
    let title_str: String = env
        .get_string(&title)
        .ok()
        .map(|s| String::from(s))
        .unwrap_or_default();
    let content_str: String = env
        .get_string(&content)
        .ok()
        .map(|s| String::from(s))
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
    mut env: JNIEnv,
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
