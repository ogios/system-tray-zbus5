use crate::dbus::dbus_menu_proxy::DBusMenuProxy;
use crate::dbus::notifier_item_proxy::StatusNotifierItemProxy;
use crate::dbus::notifier_watcher_proxy::StatusNotifierWatcherProxy;
use crate::dbus::status_notifier_watcher::StatusNotifierWatcher;
use crate::names;
use crate::stream::LoopInner;
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::timeout;
use tracing::{debug, error};
use zbus::zvariant::Value;
use zbus::Connection;

use self::names::ITEM_OBJECT;

/// A request to 'activate' one of the menu items,
/// typically sent when it is clicked.
#[derive(Debug, Clone)]
pub enum ActivateRequest {
    /// Submenu ID
    MenuItem {
        address: String,
        menu_path: String,
        submenu_id: i32,
    },
    /// Default activation for the tray.
    /// The parameter(x and y) represents screen coordinates and is to be considered an hint to the item where to show eventual windows (if any).
    Default { address: String, x: i32, y: i32 },
    /// Secondary activation(less important) for the tray.
    /// The parameter(x and y) represents screen coordinates and is to be considered an hint to the item where to show eventual windows (if any).
    Secondary { address: String, x: i32, y: i32 },
}

/// Client for watching the tray.
#[derive(Debug)]
pub struct Client {
    connection: Connection,
}

impl Client {
    pub async fn new() -> crate::error::Result<Self> {
        Ok(Self {
            connection: Connection::session().await?,
        })
    }
    /// Creates and initializes the client.
    ///
    /// The client will begin listening to items and menus and sending events immediately.
    /// It is recommended that consumers immediately follow the call to `new` with a `subscribe` call,
    /// then immediately follow that with a call to `items` to get the state to not miss any events.
    ///
    /// The value of `service_name` must be unique on the session bus.
    /// It is recommended to use something similar to the format of `appid-numid`,
    /// where `numid` is a short-ish random integer.
    ///
    /// # Errors
    ///
    /// If the initialization fails for any reason,
    /// for example if unable to connect to the bus,
    /// this method will return an error.
    ///
    /// # Panics
    ///
    /// If the generated well-known name is invalid, the library will panic
    /// as this indicates a major bug.
    ///
    /// Likewise, the spawned tasks may panic if they cannot get a `Mutex` lock.
    pub async fn create_stream(&self) -> crate::error::Result<LoopInner> {
        let connection = self.connection.clone();

        // first start server...
        StatusNotifierWatcher::new().attach_to(&connection).await?;

        // ...then connect to it
        let watcher_proxy = StatusNotifierWatcherProxy::new(&connection).await?;

        // register a host on the watcher to declare we want to watch items
        // get a well-known name
        let pid = std::process::id();
        let mut i = 0;
        let wellknown = loop {
            use zbus::fdo::RequestNameReply::*;

            i += 1;
            let wellknown = format!("org.freedesktop.StatusNotifierHost-{pid}-{i}");
            let wellknown: zbus::names::WellKnownName = wellknown
                .try_into()
                .expect("generated well-known name is invalid");

            let flags = [zbus::fdo::RequestNameFlags::DoNotQueue];
            match connection
                .request_name_with_flags(&wellknown, flags.into_iter().collect())
                .await?
            {
                PrimaryOwner => break wellknown,
                Exists | AlreadyOwner => {}
                InQueue => unreachable!(
                    "request_name_with_flags returned InQueue even though we specified DoNotQueue"
                ),
            };
        };

        debug!("wellknown: {wellknown}");
        watcher_proxy
            .register_status_notifier_host(&wellknown)
            .await?;

        let stream = watcher_proxy
            .receive_status_notifier_item_registered()
            .await?;

        Ok(LoopInner::new(connection, stream, HashMap::new()))

        // then lastly get all items
        // it can take so long to fetch all items that we have to do this last,
        // otherwise some incoming items get missed
        // {
        //     let initial_items = watcher_proxy.registered_status_notifier_items().await?;
        //     debug!("initial items: {initial_items:?}");
        //
        //     for item in initial_items {
        //         if let Err(err) =
        //             Self::handle_item(&item, connection.clone(), tx.clone(), items.clone()).await
        //         {
        //             error!("{err}");
        //         }
        //     }
        // }
    }

    async fn get_notifier_item_proxy(
        &self,
        address: String,
    ) -> crate::error::Result<StatusNotifierItemProxy<'_>> {
        let proxy = StatusNotifierItemProxy::builder(&self.connection)
            .destination(address)?
            .path(ITEM_OBJECT)?
            .build()
            .await?;
        Ok(proxy)
    }

    async fn get_menu_proxy(
        &self,
        address: String,
        menu_path: String,
    ) -> crate::error::Result<DBusMenuProxy<'_>> {
        let proxy = DBusMenuProxy::builder(&self.connection)
            .destination(address)?
            .path(menu_path)?
            .build()
            .await?;
        Ok(proxy)
    }

    /// One should call this method with id=0 when opening the root menu.
    ///
    /// ID refers to the menuitem id.
    /// Returns `needsUpdate`
    pub async fn about_to_show_menuitem(
        &self,
        address: String,
        menu_path: String,
        id: i32,
    ) -> crate::error::Result<bool> {
        let proxy = self.get_menu_proxy(address, menu_path).await?;
        Ok(proxy.about_to_show(id).await?)
    }

    /// Sends an activate request for a menu item.
    ///
    /// # Errors
    ///
    /// The method will return an error if the connection to the `DBus` object fails,
    /// or if sending the event fails for any reason.
    ///
    /// # Panics
    ///
    /// If the system time is somehow before the Unix epoch.
    pub async fn activate(&self, req: ActivateRequest) -> crate::error::Result<()> {
        macro_rules! timeout_event {
            ($event:expr) => {
                if timeout(Duration::from_secs(1), $event).await.is_err() {
                    error!("Timed out sending activate event");
                }
            };
        }
        match req {
            ActivateRequest::MenuItem {
                address,
                menu_path,
                submenu_id,
            } => {
                let proxy = self.get_menu_proxy(address, menu_path).await?;
                let timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("time should flow forwards");

                let event = proxy.event(
                    submenu_id,
                    "clicked",
                    &Value::I32(0),
                    timestamp.as_secs() as u32,
                );

                timeout_event!(event);
            }
            ActivateRequest::Default { address, x, y } => {
                let proxy = self.get_notifier_item_proxy(address).await?;
                let event = proxy.activate(x, y);

                timeout_event!(event);
            }
            ActivateRequest::Secondary { address, x, y } => {
                let proxy = self.get_notifier_item_proxy(address).await?;
                let event = proxy.secondary_activate(x, y);

                timeout_event!(event);
            }
        }

        Ok(())
    }
}
