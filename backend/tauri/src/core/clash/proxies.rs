/// This module is used to manage the proxies for the Tauri application.
/// It is used to provide the unite interface between tray and frontend.
/// TODO: add a diff algorithm to reduce the data transfer, and the rerendering of the tray menu.
use super::{api, CLASH_API_DEFAULT_BACKOFF_STRATEGY};
use anyhow::Result;
use backon::Retryable;
use log::warn;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
};
use tokio::{sync::broadcast, try_join};

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProxyGroupItem {
    pub name: String,
    pub r#type: String, // TODO: 考虑改成枚举
    pub udp: bool,
    pub history: Vec<api::ProxyItemHistory>,
    pub all: Vec<api::ProxyItem>,
    pub now: Option<String>, // 当前选中的代理
    pub provider: Option<String>,
    pub alive: Option<bool>, // Mihomo Or Premium Only
    #[serde(skip_serializing_if = "Option::is_none")]
    pub xudp: Option<bool>, // Mihomo Only
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tfo: Option<bool>, // Mihomo Only
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>, // Mihomo Only
    #[serde(default)]
    pub hidden: bool, // Mihomo Only
                             // extra: {}, // Mihomo Only
}

impl From<api::ProxyItem> for ProxyGroupItem {
    fn from(item: api::ProxyItem) -> Self {
        let all = vec![];
        ProxyGroupItem {
            name: item.name,
            r#type: item.r#type,
            udp: item.udp,
            history: item.history,
            all,
            now: item.now,
            provider: item.provider,
            alive: item.alive,
            xudp: item.xudp,
            tfo: item.tfo,
            icon: item.icon,
            hidden: item.hidden,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Proxies {
    pub global: ProxyGroupItem,
    pub direct: api::ProxyItem,
    pub groups: Vec<ProxyGroupItem>,
    pub records: HashMap<String, api::ProxyItem>,
    pub proxies: Vec<api::ProxyItem>,
}

async fn fetch_proxies() -> Result<(api::ProxiesRes, api::ProvidersProxiesRes)> {
    try_join!(api::get_proxies(), api::get_providers_proxies())
}

impl Proxies {
    pub async fn fetch() -> Result<Self> {
        let (inner_proxies, providers_proxies) = fetch_proxies
            .retry(&*CLASH_API_DEFAULT_BACKOFF_STRATEGY)
            .await?;
        let inner_proxies = inner_proxies.proxies.unwrap_or_default();
        // 1. filter out the Http or File type provider proxies
        let providers_proxies: HashMap<String, api::ProxyProviderItem> = {
            let records = providers_proxies.providers.unwrap_or_default();
            records
                .into_iter()
                .filter(|(_k, v)| {
                    matches!(
                        v.vehicle_type,
                        api::VehicleType::Http | api::VehicleType::File
                    )
                })
                .collect()
        };

        // 2. mapping provider => providerProxiesItem to name => ProxyItem
        let mut provider_map = HashMap::<String, api::ProxyItem>::new();
        for (provider, record) in providers_proxies.iter() {
            let name = record.name.clone();
            let mut record: api::ProxyItem = record.clone().into();
            record.provider = Some(provider.clone());
            provider_map.insert(name, record);
        }
        let generate_item = |name: &str| {
            if let Some(r) = inner_proxies.get(name) {
                r.clone()
            } else if let Some(r) = provider_map.get(name) {
                r.clone()
            } else {
                api::ProxyItem {
                    name: name.to_string(),
                    r#type: "Unknown".to_string(),
                    udp: false,
                    history: vec![],
                    ..Default::default()
                }
            }
        };

        let global = inner_proxies.get("GLOBAL");
        let direct = inner_proxies
            .get("DIRECT")
            .ok_or(anyhow::anyhow!("DIRECT is missing in /proxies"))?
            .clone(); // It should be always exists
        let reject = inner_proxies
            .get("REJECT")
            .ok_or(anyhow::anyhow!("REJECT is missing in /proxies"))?
            .clone(); // It should be always exists

        // 3. generate the proxies groups
        let groups: Vec<ProxyGroupItem> = match global {
            Some(api::ProxyItem { all: Some(all), .. }) => {
                let all = all.clone();
                all.into_iter()
                    .filter(|name| {
                        matches!(
                            inner_proxies.get(name),
                            Some(api::ProxyItem { all: Some(_), .. })
                        )
                    })
                    .map(|name| {
                        let item = inner_proxies
                            .get(&name)
                            .unwrap_or(&api::ProxyItem::default())
                            .clone();
                        let item_all = item.all.clone().unwrap_or_default();
                        let mut item: ProxyGroupItem = item.into();
                        item.all = item_all
                            .into_iter()
                            .map(|name| generate_item(&name))
                            .collect();
                        item
                    })
                    .collect()
            }
            _ => {
                let mut groups: Vec<ProxyGroupItem> = inner_proxies
                    .clone()
                    .into_values()
                    .filter(|v| v.name == "GLOBAL" && v.all.is_some())
                    .map(|v| {
                        let all = v.all.clone().unwrap_or_default();
                        let mut item: ProxyGroupItem = v.clone().into();
                        item.all = all.into_iter().map(|name| generate_item(&name)).collect();
                        item
                    })
                    .collect();
                groups.sort_by(|a, b| b.name.to_lowercase().cmp(&a.name.to_lowercase()));
                groups
            }
        };

        // 4. generate the proxies
        let mut proxies: Vec<api::ProxyItem> = vec![direct.clone(), reject];
        proxies.extend(inner_proxies.clone().into_values().filter(|v| {
            matches!(v.name.as_str(), "DIRECT" | "REJECT")
                && (v.all.is_none() || v.all.as_ref().unwrap().is_empty())
        }));

        // 5. generate the global
        let global: Option<ProxyGroupItem> = global.map(|v| {
            let all = v.all.clone().unwrap_or_default();
            let mut item: ProxyGroupItem = v.clone().into();
            item.all = all.into_iter().map(|name| generate_item(&name)).collect();
            item
        });

        Ok(Proxies {
            global: global.unwrap_or_default(),
            direct,
            groups,
            records: inner_proxies,
            proxies,
        })
    }
}

pub struct ProxiesGuard {
    inner: Proxies,
    updated_at: u64,
    sender: broadcast::Sender<()>,
}

impl ProxiesGuard {
    pub fn global() -> &'static Arc<RwLock<ProxiesGuard>> {
        static PROXIES: OnceLock<Arc<RwLock<ProxiesGuard>>> = OnceLock::new();
        PROXIES.get_or_init(|| {
            let (tx, _) = broadcast::channel(5); // 默认提供 5 个消费位置，提供一定的缓冲
            Arc::new(RwLock::new(ProxiesGuard {
                sender: tx,
                inner: Proxies::default(),
                updated_at: 0,
            }))
        })
    }

    pub fn get_receiver(&self) -> broadcast::Receiver<()> {
        self.sender.subscribe()
    }

    pub fn replace(&mut self, proxies: Proxies) {
        let now = tokio::time::Instant::now().elapsed().as_secs();
        self.inner = proxies;
        self.updated_at = now;

        if let Err(e) = self.sender.send(()) {
            warn!(
                target: "clash::proxies",
                "send update signal failed: {:?}", e
            );
        }
    }

    // pub async fn select_proxy(&mut self, group: &str, name: &str) -> Result<()> {
    //     api::update_proxy(group, name).await?;
    //     self.update().await?;
    //     Ok(())
    // }

    pub fn inner(&self) -> &Proxies {
        &self.inner
    }

    pub fn updated_at(&self) -> u64 {
        self.updated_at
    }
}

pub trait ProxiesGuardExt {
    async fn update(&self) -> Result<()>;
    async fn select_proxy(&self, group: &str, name: &str) -> Result<()>;
}

type ProxiesGuardSingleton = &'static Arc<RwLock<ProxiesGuard>>;
impl ProxiesGuardExt for ProxiesGuardSingleton {
    async fn update(&self) -> Result<()> {
        let proxies = Proxies::fetch().await?;
        {
            self.write().replace(proxies);
        }
        Ok(())
    }

    async fn select_proxy(&self, group: &str, name: &str) -> Result<()> {
        api::update_proxy(group, name).await?;
        self.update().await?;
        Ok(())
    }
}
