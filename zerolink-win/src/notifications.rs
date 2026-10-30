use windows::core::{HSTRING, Result as WinResult};
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use zerolink_core::protocol::NotificationPayload;

pub struct WindowsToastManager {
    app_id: HSTRING,
}

impl WindowsToastManager {
    pub fn new(app_id: &str) -> Self {
        Self {
            app_id: HSTRING::from(app_id),
        }
    }

    /// Renders a native Windows Toast directly via WinRT COM without any Electron or CEF dependency.
    pub fn display_notification(&self, payload: &NotificationPayload) -> WinResult<()> {
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
                    <action content="Reply" arguments="action=reply&amp;id={}" activationType="background" />
                    <action content="Dismiss" arguments="action=dismiss&amp;id={}" activationType="background" />
                </actions>
            </toast>"#,
            escape_xml(&payload.title),
            escape_xml(&payload.content),
            escape_xml(&payload.app_name),
            escape_xml(&payload.id),
            escape_xml(&payload.id),
        );

        let xml_doc = XmlDocument::new()?;
        xml_doc.LoadXml(&HSTRING::from(xml_template))?;

        let toast = ToastNotification::CreateToastNotification(&xml_doc)?;
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&self.app_id)?;

        notifier.Show(&toast)?;
        Ok(())
    }
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
