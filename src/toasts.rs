//! One poll of `UserNotificationListener`. No queue and no timer here.
//!
//! The view's 1s loop calls [`poll`] on `background_executor`. Access denied
//! and any `WinRT` failure become an empty `Vec`, which the view treats as
//! "the listener reported nothing", not as an error page.
//!
//! [`read`] asks for toast notifications, pulls app name plus text nodes into
//! a [`rustedge::notice::Notice`], and stamps `arrival_ms`. The first result
//! the view sees is the Action Center backlog: those ids are recorded and
//! not shown. Later polls only surface ids the view has not seen. The queue
//! in `notice` decides preemption; this file only reads.

use rustedge::notice::Notice;

/// Visible text when a toast has no text elements.
#[must_use]
pub fn fallback_title(app: &str) -> String {
    let mut text = String::new();
    text.push_str(app);
    text.push_str("发来一条通知");
    text
}

/// One poll. Access denied and any `WinRT` failure yield an empty list.
pub fn poll() -> Vec<Notice> {
    read().unwrap_or_default()
}

fn toast_texts(item: &windows::UI::Notifications::UserNotification) -> Vec<String> {
    let Ok(notification) = item.Notification() else { return Vec::new() };
    let Ok(visual) = notification.Visual() else { return Vec::new() };
    let Ok(bindings) = visual.Bindings() else { return Vec::new() };
    let mut texts = Vec::new();
    for binding in bindings {
        let Ok(elements) = binding.GetTextElements() else { continue };
        for element in elements {
            if let Ok(text) = element.Text() {
                let text = text.to_os_string().to_string_lossy().into_owned();
                if !text.is_empty() {
                    texts.push(text);
                }
            }
        }
    }
    texts
}

fn read() -> windows::core::Result<Vec<Notice>> {
    use windows::UI::Notifications::Management::UserNotificationListener;
    use windows::UI::Notifications::NotificationKinds;
    let listener = UserNotificationListener::Current()?;
    let access = listener.RequestAccessAsync()?.join()?;
    if access != windows::UI::Notifications::Management::UserNotificationListenerAccessStatus::Allowed {
        return Ok(Vec::new());
    }
    let list = listener.GetNotificationsAsync(NotificationKinds::Toast)?.join()?;
    let mut notices = Vec::new();
    for item in list {
        let id = item.Id().unwrap_or(0);
        let app = item.AppInfo().and_then(|info| info.DisplayInfo())
            .and_then(|info| info.DisplayName())
            .map_or_else(|_| "通知".to_owned(), |name| name.to_os_string().to_string_lossy().into_owned());
        let texts = toast_texts(&item);
        let parsed_title = texts.first().cloned().unwrap_or_default();
        let body = texts.iter().skip(1).cloned().collect::<Vec<_>>().join("\n");
        let title = if parsed_title.is_empty() { fallback_title(&app) } else { parsed_title };
        notices.push(Notice { id, app, title, body, arrival_ms: millis(), local: false });
    }
    Ok(notices)
}

fn millis() -> u64 {
    u64::try_from(
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis()),
    ).unwrap_or(0)
}
