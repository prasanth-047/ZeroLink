use std::sync::Mutex;

use tokio::sync::mpsc::UnboundedSender;
use windows::core::{Interface, Result as WinResult, HSTRING};
use windows::ApplicationModel::Activation::ToastNotificationActivatedEventArgs;
use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::Collections::ValueSet;
use windows::Foundation::{IPropertyValue, IReference, TypedEventHandler};
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use zerolink_core::protocol::{NotificationActionPayload, NotificationPayload};

/// Maximum number of live toast objects retained for activation callbacks.
const MAX_LIVE_TOASTS: usize = 32;

struct LiveToast {
    _toast: ToastNotification,
    _handler: TypedEventHandler<ToastNotification, windows::core::IInspectable>,
}

// WinRT delegates are agile (free-threaded), but the generated `TypedEventHandler`
// wrapper stores a raw interface pointer and is therefore not automatically `Send`.
// The handler only captures an `UnboundedSender`, so moving it between threads is safe.
unsafe impl Send for LiveToast {}
unsafe impl Sync for LiveToast {}

pub struct WindowsToastManager {
    app_id: HSTRING,
    action_tx: UnboundedSender<NotificationActionPayload>,
    live_toasts: Mutex<Vec<LiveToast>>,
}

impl WindowsToastManager {
    pub fn new(app_id: &str, action_tx: UnboundedSender<NotificationActionPayload>) -> Self {
        Self {
            app_id: HSTRING::from(app_id),
            action_tx,
            live_toasts: Mutex::new(Vec::new()),
        }
    }

    /// Renders a native Windows Toast directly via WinRT COM without any Electron or CEF dependency.
    /// The toast object is retained so that `Activated` events (inline quick replies) can fire.
    pub fn display_notification(&self, payload: &NotificationPayload) -> WinResult<()> {
        let escaped_id = escape_xml(&payload.id);
        let xml_template = format!(
            r#"<toast duration="short">
                <visual>
                    <binding template="ToastGeneric">
                        <text hint-maxLines="1">{}</text>
                        <text>{}</text>
                        <text placement="attribution">{}</text>
                    </binding>
                </visual>
                <actions>
                    <input id="quickReply" type="text" placeHolderContent="Type a reply..." />
                    <action content="Reply" arguments="action=reply&amp;id={}" activationType="background" hint-inputId="quickReply" />
                    <action content="Dismiss" arguments="action=dismiss&amp;id={}" activationType="background" />
                </actions>
            </toast>"#,
            escape_xml(&payload.title),
            escape_xml(&payload.content),
            escape_xml(&payload.app_name),
            escaped_id,
            escaped_id,
        );

        let xml_doc = XmlDocument::new()?;
        xml_doc.LoadXml(&HSTRING::from(xml_template))?;

        let toast = ToastNotification::CreateToastNotification(&xml_doc)?;

        // Keep the activation handler and the toast itself alive for as long as the
        // notification is actionable. WinRT only raises `Activated` while the object
        // is referenced, so the previous fire-and-forget implementation could never
        // deliver inline replies.
        let action_tx = self.action_tx.clone();
        let handler = TypedEventHandler::<ToastNotification, windows::core::IInspectable>::new(
            move |_sender, args| {
                let Some(args) = args.as_ref() else {
                    return Ok(());
                };
                let Ok(args) = args.cast::<ToastNotificationActivatedEventArgs>() else {
                    return Ok(());
                };

                let arguments = args.Argument().map(|s| s.to_string()).unwrap_or_default();
                let (action, notification_id) = parse_arguments(&arguments);
                if action.is_empty() {
                    return Ok(());
                }

                let reply_text = if action == "reply" {
                    args.UserInput()
                        .ok()
                        .map(|input| read_user_input(&input, "quickReply"))
                        .unwrap_or_default()
                } else {
                    String::new()
                };

                let _ = action_tx.send(NotificationActionPayload {
                    notification_id,
                    action_id: action,
                    reply_text,
                });
                Ok(())
            },
        );

        toast.Activated(&handler)?;

        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&self.app_id)?;
        notifier.Show(&toast)?;

        let mut live = match self.live_toasts.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        live.push(LiveToast {
            _toast: toast,
            _handler: handler,
        });
        if live.len() > MAX_LIVE_TOASTS {
            live.remove(0);
        }

        Ok(())
    }
}

/// Parses toast action arguments of the shape `action=reply&id=<notification-id>`.
fn parse_arguments(arguments: &str) -> (String, String) {
    let mut action = String::new();
    let mut id = String::new();

    for pair in arguments.split('&') {
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or_default();
        let value = parts.next().unwrap_or_default();
        match key {
            "action" => action = percent_decode(value),
            "id" => id = percent_decode(value),
            _ => {}
        }
    }

    (action, id)
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or_default();
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn read_user_input(input: &ValueSet, key: &str) -> String {
    let Ok(value) = input.Lookup(&HSTRING::from(key)) else {
        return String::new();
    };

    if let Ok(property) = value.cast::<IPropertyValue>() {
        if let Ok(text) = property.GetString() {
            return text.to_string();
        }
    }

    if let Ok(reference) = value.cast::<IReference<HSTRING>>() {
        if let Ok(text) = reference.Value() {
            return text.to_string();
        }
    }

    String::new()
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_toast_arguments() {
        let (action, id) = parse_arguments("action=reply&id=0%7Ccom.whatsapp%7C123");
        assert_eq!(action, "reply");
        assert_eq!(id, "0|com.whatsapp|123");

        let (action, id) = parse_arguments("action=dismiss&id=abc");
        assert_eq!(action, "dismiss");
        assert_eq!(id, "abc");
    }
}
