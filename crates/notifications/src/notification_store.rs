use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, Task};
use rpc::{Notification, proto};
use std::ops::Range;
use sum_tree::{Bias, Dimensions, SumTree};
use time::OffsetDateTime;

pub fn init(cx: &mut App) {
    let notification_store = cx.new(|cx| NotificationStore::new(cx));
    cx.set_global(GlobalNotificationStore(notification_store));
}

struct GlobalNotificationStore(Entity<NotificationStore>);

impl Global for GlobalNotificationStore {}

pub struct NotificationStore {
    notifications: SumTree<NotificationEntry>,
    loaded_all_notifications: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum NotificationEvent {
    NotificationsUpdated {
        old_range: Range<usize>,
        new_count: usize,
    },
    NewNotification {
        entry: NotificationEntry,
    },
    NotificationRemoved {
        entry: NotificationEntry,
    },
    NotificationRead {
        entry: NotificationEntry,
    },
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct NotificationEntry {
    pub id: u64,
    pub notification: Notification,
    pub timestamp: OffsetDateTime,
    pub is_read: bool,
    pub response: Option<bool>,
}

#[derive(Clone, Debug, Default)]
pub struct NotificationSummary {
    max_id: u64,
    count: usize,
    unread_count: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Count(usize);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct NotificationId(u64);

impl NotificationStore {
    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalNotificationStore>().0.clone()
    }

    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            notifications: Default::default(),
            loaded_all_notifications: false,
        }
    }

    pub fn notification_count(&self) -> usize {
        self.notifications.summary().count
    }

    pub fn unread_notification_count(&self) -> usize {
        self.notifications.summary().unread_count
    }

    // Get the nth newest notification.
    pub fn notification_at(&self, ix: usize) -> Option<&NotificationEntry> {
        let count = self.notifications.summary().count;
        if ix >= count {
            return None;
        }
        let ix = count - 1 - ix;
        let (.., item) = self
            .notifications
            .find::<Count, _>((), &Count(ix), Bias::Right);
        item
    }
    pub fn notification_for_id(&self, id: u64) -> Option<&NotificationEntry> {
        let (.., item) =
            self.notifications
                .find::<NotificationId, _>((), &NotificationId(id), Bias::Left);
        if let Some(item) = item
            && item.id == id
        {
            return Some(item);
        }
        None
    }

    // load_more_notifications removed: requires client to fetch from server
    // fn load_more_notifications(&self, clear_old: bool, cx: &mut Context<Self>) -> Option<Task<Result<()>>> { … }

    // handle_connect removed: requires client connection status
    // fn handle_connect(&mut self, cx: &mut Context<Self>) -> Option<Task<Result<()>>> { … }

    // handle_disconnect removed: requires client connection status
    // fn handle_disconnect(&mut self, cx: &mut Context<Self>) { … }

    // handle_new_notification removed: requires client message handler
    // async fn handle_new_notification(this, envelope, cx) -> Result<()> { … }

    // handle_delete_notification removed: requires client message handler
    // async fn handle_delete_notification(this, envelope, cx) -> Result<()> { … }

    // add_notifications removed: was only called by client message handlers and load_more_notifications which require client
    // async fn add_notifications(this, notifications, options, cx) -> Result<()> { … }

    fn splice_notifications(
        &mut self,
        notifications: impl IntoIterator<Item = (u64, Option<NotificationEntry>)>,
        is_new: bool,
        cx: &mut Context<NotificationStore>,
    ) {
        let mut cursor = self
            .notifications
            .cursor::<Dimensions<NotificationId, Count>>(());
        let mut new_notifications = SumTree::default();
        let mut old_range = 0..0;

        for (i, (id, new_notification)) in notifications.into_iter().enumerate() {
            new_notifications.append(cursor.slice(&NotificationId(id), Bias::Left), ());

            if i == 0 {
                old_range.start = cursor.start().1.0;
            }

            let old_notification = cursor.item();
            if let Some(old_notification) = old_notification {
                if old_notification.id == id {
                    cursor.next();

                    if let Some(new_notification) = &new_notification {
                        if new_notification.is_read {
                            cx.emit(NotificationEvent::NotificationRead {
                                entry: new_notification.clone(),
                            });
                        }
                    } else {
                        cx.emit(NotificationEvent::NotificationRemoved {
                            entry: old_notification.clone(),
                        });
                    }
                }
            } else if let Some(new_notification) = &new_notification
                && is_new
            {
                cx.emit(NotificationEvent::NewNotification {
                    entry: new_notification.clone(),
                });
            }

            if let Some(notification) = new_notification {
                new_notifications.push(notification, ());
            }
        }

        old_range.end = cursor.start().1.0;
        let new_count = new_notifications.summary().count - old_range.start;
        new_notifications.append(cursor.suffix(), ());
        drop(cursor);

        self.notifications = new_notifications;
        cx.emit(NotificationEvent::NotificationsUpdated {
            old_range,
            new_count,
        });
    }

    pub fn respond_to_notification(
        &mut self,
        _notification: Notification,
        _response: bool,
        _cx: &mut Context<Self>,
    ) {
        // respond_to_notification removed: requires user_store and channel_store
    }
}

impl EventEmitter<NotificationEvent> for NotificationStore {}

impl sum_tree::Item for NotificationEntry {
    type Summary = NotificationSummary;

    fn summary(&self, _cx: ()) -> Self::Summary {
        NotificationSummary {
            max_id: self.id,
            count: 1,
            unread_count: if self.is_read { 0 } else { 1 },
        }
    }
}

impl sum_tree::ContextLessSummary for NotificationSummary {
    fn zero() -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &Self) {
        self.max_id = self.max_id.max(summary.max_id);
        self.count += summary.count;
        self.unread_count += summary.unread_count;
    }
}

impl sum_tree::Dimension<'_, NotificationSummary> for NotificationId {
    fn zero(_cx: ()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &NotificationSummary, _: ()) {
        debug_assert!(summary.max_id > self.0);
        self.0 = summary.max_id;
    }
}

impl sum_tree::Dimension<'_, NotificationSummary> for Count {
    fn zero(_cx: ()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &NotificationSummary, _: ()) {
        self.0 += summary.count;
    }
}

struct AddNotificationsOptions {
    is_new: bool,
    clear_old: bool,
    includes_first: bool,
}
