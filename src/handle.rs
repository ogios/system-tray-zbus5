use tracing::{debug, error, warn};
use zbus::{
    fdo::{DBusProxy, PropertiesProxy},
    names::InterfaceName,
    zvariant::Structure,
    Message,
};

use crate::{
    client::UpdateEvent,
    dbus::{
        dbus_menu_proxy::DBusMenuProxy, notifier_item_proxy::StatusNotifierItemProxy,
        notifier_watcher_proxy::StatusNotifierWatcherProxy, DBusProps, OwnedValueExt,
    },
    error::Result,
    item::{self, StatusNotifierItem},
    menu::TrayMenu,
    stream::{Item, LoopInner, Token},
};

#[derive(Debug, Clone)]
pub struct Event {
    pub destination: String,
    pub t: EventType,
}
impl Event {
    pub(crate) fn new(destination: String, t: EventType) -> Self {
        Self { destination, t }
    }
}

#[derive(Debug, Clone)]
pub enum EventType {
    /// A new `StatusNotifierItem` was added.
    Add(Box<StatusNotifierItem>),
    /// An update was received for an existing `StatusNotifierItem`.
    /// This could be either an update to the item itself,
    /// or an update to the associated menu.
    Update(UpdateEvent),
    /// A `StatusNotifierItem` was unregistered.
    Remove,
}

#[derive(Debug)]
struct LoopEventAddInner {
    events: Vec<Event>,
    token: Token,
    item: Item,
}

#[derive(Debug)]
pub(crate) enum LoopEvent {
    Add(Box<LoopEventAddInner>),
    Remove(Event),
    Updata(Event),
}
impl LoopEvent {
    pub(crate) fn process_by_loop(self, lp: &mut LoopInner) -> Vec<Event> {
        match self {
            LoopEvent::Add(inner) => {
                let LoopEventAddInner {
                    events,
                    token,
                    item,
                } = *inner;
                lp.wrap_new_item_added(events, token, item)
            }
            LoopEvent::Remove(event) => {
                lp.item_removed(&event.destination);
                vec![event]
            }
            LoopEvent::Updata(event) => vec![event],
        }
    }
}

async fn get_update_event(
    change: Message,
    properties_proxy: &PropertiesProxy<'_>,
) -> Option<UpdateEvent> {
    let header = change.header();
    let member = header.member()?;

    let property_name = match member.as_str() {
        "NewAttentionIcon" => "AttentionIconName",
        "NewIcon" => "IconName",
        "NewOverlayIcon" => "OverlayIconName",
        "NewStatus" => "Status",
        "NewTitle" => "Title",
        "NewToolTip" => "ToolTip",
        _ => &member.as_str()["New".len()..],
    };

    let res = properties_proxy
        .get(
            InterfaceName::from_static_str(PROPERTIES_INTERFACE)
                .expect("to be valid interface name"),
            property_name,
        )
        .await;

    let property = match res {
        Ok(property) => property,
        Err(err) => {
            error!("error fetching property '{property_name}': {err:?}");
            return None;
        }
    };

    debug!("received tray item update: {member} -> {property:?}");

    use UpdateEvent::*;
    match member.as_str() {
        "NewAttentionIcon" => Some(AttentionIcon(property.to_string().ok())),
        "NewIcon" => Some(Icon(property.to_string().ok())),
        "NewOverlayIcon" => Some(OverlayIcon(property.to_string().ok())),
        "NewStatus" => Some(Status(
            property
                .downcast_ref::<&str>()
                .ok()
                .map(item::Status::from)
                .unwrap_or_default(),
        )),
        "NewTitle" => Some(Title(property.to_string().ok())),
        "NewToolTip" => Some(Tooltip({
            property
                .downcast_ref::<&Structure>()
                .ok()
                .map(crate::item::Tooltip::try_from)?
                .ok()
        })),
        _ => {
            warn!("received unhandled update event: {member}");
            None
        }
    }
}

async fn get_new_layout(destination: String, proxy: &DBusMenuProxy<'static>) -> Result<Event> {
    let menu = proxy.get_layout(0, -1, &[]).await?;
    let menu = TrayMenu::try_from(menu)?;
    Ok(Event::new(
        destination.clone(),
        EventType::Update(UpdateEvent::Menu(menu)),
    ))
}

pub(crate) async fn to_update_item_event(
    destination: String,
    m: Message,
    proxy: PropertiesProxy<'static>,
) -> Option<LoopEvent> {
    let e = get_update_event(m, &proxy).await?;
    Some(LoopEvent::Updata(Event::new(
        destination,
        EventType::Update(e),
    )))
}

pub(crate) async fn to_layout_update_event(
    destination: String,
    proxy: DBusMenuProxy<'static>,
) -> Option<LoopEvent> {
    let e = get_new_layout(destination, &proxy)
        .await
        .inspect_err(|e| error!("error get layout: {e}"))
        .ok()?;

    Some(LoopEvent::Updata(e))
}

const PROPERTIES_INTERFACE: &str = "org.kde.StatusNotifierItem";

async fn get_item_properties(
    destination: &str,
    path: &str,
    properties_proxy: &PropertiesProxy<'_>,
) -> Result<StatusNotifierItem> {
    let properties = properties_proxy
        .get_all(
            InterfaceName::from_static_str(PROPERTIES_INTERFACE)
                .expect("to be valid interface name"),
        )
        .await;

    let properties = match properties {
        Ok(properties) => properties,
        Err(err) => {
            error!("Error fetching properties from {destination}{path}: {err:?}");
            return Err(err.into());
        }
    };

    StatusNotifierItem::try_from(DBusProps(properties))
}

impl LoopInner {
    pub(crate) fn handle_new_item(
        &mut self,
        item: crate::dbus::notifier_watcher_proxy::StatusNotifierItemRegistered,
    ) -> Vec<Event> {
        let connection = self.connection.clone();

        let fut = async move {
            let res: Result<LoopEvent> = async {
                let address = item.args().map(|args| args.service)?;

                let (destination, path) = parse_address(address);

                let properties_proxy = PropertiesProxy::builder(&connection)
                    .destination(destination.to_string())?
                    .path(path.clone())?
                    .build()
                    .await?;

                let properties = get_item_properties(destination, &path, &properties_proxy).await?;

                let mut events = vec![Event::new(
                    destination.to_string(),
                    EventType::Add(properties.clone().into()),
                )];

                let notifier_item_proxy = StatusNotifierItemProxy::builder(&connection)
                    .destination(destination)?
                    .path(path.clone())?
                    .build()
                    .await?;
                let dbus_proxy = DBusProxy::new(&connection).await?;
                let disconnect_stream = dbus_proxy.receive_name_owner_changed().await?;
                let property_change_stream =
                    notifier_item_proxy.inner().receive_all_signals().await?;

                let (layout_updated_stream, dbus_menu_proxy) =
                    if let Some(menu_path) = properties.menu {
                        let destination = destination.to_string();

                        let dbus_menu_proxy = DBusMenuProxy::builder(&connection)
                            .destination(destination.clone())?
                            .path(menu_path)?
                            .build()
                            .await?;

                        events.push(get_new_layout(destination, &dbus_menu_proxy).await?);

                        (
                            Some(dbus_menu_proxy.receive_layout_updated().await?),
                            Some(dbus_menu_proxy),
                        )
                    } else {
                        (None, None)
                    };

                Ok(LoopEvent::Add(Box::new(LoopEventAddInner {
                    events,
                    token: Token::new(destination.to_string()),
                    item: Item {
                        properties_proxy,
                        dbus_menu_proxy,
                        disconnect_stream,
                        property_change_stream,
                        layout_updated_stream,
                    },
                })))
            }
            .await
            .inspect_err(|e| tracing::error!("Fail to handle_new_item: {e}"));

            res.ok()
        };

        self.futures
            .try_put(fut, self.waker_data.clone().unwrap())
            .map(|e| e.process_by_loop(self))
            .unwrap_or_default()
    }

    pub(crate) fn wrap_new_item_added(
        &mut self,
        mut events: Vec<Event>,
        token: Token,
        item: Item,
    ) -> Vec<Event> {
        let mut e = self.new_item_added(token, item);
        events.append(&mut e);
        events
    }

    pub(crate) fn new_item_added(&mut self, token: Token, mut item: Item) -> Vec<Event> {
        let mut es = vec![];

        // watch property
        let property_changed = item.poll_property_change(
            token.clone(),
            self.waker_data.clone().unwrap(),
            &mut self.futures,
        );
        es.append(
            &mut property_changed
                .into_iter()
                .flat_map(|e| e.process_by_loop(self))
                .collect(),
        );

        // watch layout
        let layout_changed = item.poll_layout_change(
            token.clone(),
            self.waker_data.clone().unwrap(),
            &mut self.futures,
        );
        es.append(
            &mut layout_changed
                .into_iter()
                .flat_map(|e| e.process_by_loop(self))
                .collect(),
        );

        // watch disconnect
        let disconnect_stream =
            item.poll_disconnect(token.clone(), self.waker_data.clone().unwrap());

        self.items.insert(token.clone(), item);

        es.append(
            &mut disconnect_stream
                .into_iter()
                .flat_map(|d| self.handle_remove_item(&token, d))
                .collect(),
        );

        es
    }

    pub(crate) fn handle_remove_item(
        &mut self,
        token: &Token,
        n: zbus::fdo::NameOwnerChanged,
    ) -> Vec<Event> {
        let destination = token.destination.clone();
        let connection = self.connection.clone();

        let fut = async move {
            let args = n
                .args()
                .inspect_err(|e| error!("Failed to parse NameOwnerChanged: {e:?}"))
                .ok()
                .unwrap();
            let old = args.old_owner();
            let new = args.new_owner();

            if let (Some(old), None) = (old.as_ref(), new.as_ref()) {
                if old == destination.as_str() {
                    debug!("[{destination}] disconnected");

                    let watcher_proxy = StatusNotifierWatcherProxy::new(&connection)
                        .await
                        .expect("Failed to open StatusNotifierWatcherProxy");

                    if let Err(error) = watcher_proxy.unregister_status_notifier_item(old).await {
                        error!("{error:?}");
                    }

                    return Some(LoopEvent::Remove(Event::new(
                        destination.to_string(),
                        EventType::Remove,
                    )));
                };
            }

            None
        };

        self.futures
            .try_put(fut, self.waker_data.clone().unwrap())
            .map(|e| e.process_by_loop(self))
            .unwrap_or_default()
    }

    pub(crate) fn item_removed(&mut self, destination: &str) {
        self.items.remove(&Token::new(destination.to_string()));
    }
}

fn parse_address(address: &str) -> (&str, String) {
    address
        .split_once('/')
        .map_or((address, String::from("/StatusNotifierItem")), |(d, p)| {
            (d, format!("/{p}"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_unnamed() {
        let address = ":1.58/StatusNotifierItem";
        let (destination, path) = parse_address(address);

        assert_eq!(":1.58", destination);
        assert_eq!("/StatusNotifierItem", path);
    }

    #[test]
    fn parse_named() {
        let address = ":1.72/org/ayatana/NotificationItem/dropbox_client_1398";
        let (destination, path) = parse_address(address);

        assert_eq!(":1.72", destination);
        assert_eq!("/org/ayatana/NotificationItem/dropbox_client_1398", path);
    }
}
