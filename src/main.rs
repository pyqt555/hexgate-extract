use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
    collections::{HashMap, HashSet},
    rc::Rc,
};

use anyhow::{anyhow, Result, Context};
use bytes::Bytes;
use slint::{ModelRc, SharedString, VecModel};
use hexgate::wrapper::types as hexgate;
// Import the correct compiled types from your hexgate library
use ::hexgate::wrapper::types::{
    Install, InstallationChunkSource, LeagueVersion, ManifestHl, ManifestWrapper, FolderTreeNode, WBundleSource, Data
};

slint::include_modules!();

// Max items rendered inside Slint's scroll viewport (keeps layout calculations instant)
const MAX_DISPLAY_ROWS: usize = 300;

// =========================================================================
// 1. DATA MODELS, UTILITIES & LOCAL MANIFEST FETCH LOGIC
// =========================================================================

#[derive(Debug, Clone, serde::Deserialize)]
pub struct CatalogEntry {
    pub version: String,
    pub realms: Vec<String>,
    pub platform: Option<String>,
    pub platforms: Option<Vec<String>>,
    pub artifact_type: Option<String>,
    pub timestamp: String,
    pub size: u64,
}

impl CatalogEntry {
    pub fn get_platforms(&self) -> Vec<String> {
        if let Some(ref list) = self.platforms {
            list.clone()
        } else if let Some(ref single) = self.platform {
            vec![single.clone()]
        } else {
            vec!["neutral".to_string()]
        }
    }
}

fn build_manifest_cache(catalog: &HashMap<String, HashMap<String, CatalogEntry>>) -> HashMap<String, GameCache> {
    let mut cache = HashMap::new();
    for (game, manifests) in catalog {
        let mut list = Vec::new();
        let mut platforms_set: HashSet<String> = HashSet::new();
        let mut realms_set: HashSet<String> = HashSet::new();
        let mut artifacts_set: HashSet<String> = HashSet::new();

        for (id, entry) in manifests {
            for p in entry.get_platforms() { platforms_set.insert(p); }
            for r in &entry.realms { realms_set.insert(r.clone()); }
            if let Some(ref art) = entry.artifact_type {
                if art != "default" { artifacts_set.insert(art.clone()); }
            }

            let short_v = if entry.version.len() > 16 {
                format!("{}...", &entry.version[..14])
            } else {
                entry.version.clone()
            };

            let realms_short = if entry.realms.len() > 3 {
                format!("{}, {} (+{})", entry.realms[0], entry.realms[1], entry.realms.len() - 2)
            } else {
                entry.realms.join(", ")
            };

            list.push(CachedManifest {
                id: id.clone(),
                version: entry.version.clone(),
                version_short: short_v,
                realms: entry.realms.clone(),
                realms_short,
                platforms: entry.get_platforms(),
                artifact_type: entry.artifact_type.clone().unwrap_or_else(|| "default".to_string()),
                date_str: format_date(&entry.timestamp),
                size_str: format_size(entry.size),
                size: entry.size,
                timestamp: entry.timestamp.clone(),
            });
        }

        list.sort_unstable_by(|a, b| b.timestamp.cmp(&a.timestamp));

        let mut unique_platforms: Vec<String> = platforms_set.into_iter().collect();
        unique_platforms.sort();
        let mut unique_realms: Vec<String> = realms_set.into_iter().collect();
        unique_realms.sort();
        let mut unique_artifacts: Vec<String> = artifacts_set.into_iter().collect();
        unique_artifacts.sort();

        cache.insert(game.clone(), GameCache {
            manifests: list,
            unique_platforms,
            unique_realms,
            unique_artifacts,
        });
    }
    cache
}

// Flat pre-cached data structure designed for high-performance sorting and searches
#[derive(Clone, Debug)]
struct CachedManifest {
    id: String,
    version: String,
    version_short: String,
    realms: Vec<String>,
    realms_short: String,
    platforms: Vec<String>,
    artifact_type: String, // Defaults to "default" if None
    date_str: String,
    size_str: String,
    size: u64,
    timestamp: String,
}

// Pre-computed lists generated ONCE during file load/sync, completely bypassing runtime loop lag
#[derive(Clone, Debug)]
struct GameCache {
    manifests: Vec<CachedManifest>,
    unique_platforms: Vec<String>,
    unique_realms: Vec<String>,
    unique_artifacts: Vec<String>,
}

// Convert "2026-01-15T03:20:21+00:00" -> "15/01/26"
fn format_date(iso_str: &str) -> String {
    if iso_str.len() < 10 {
        return "??/??/??".to_string();
    }
    let parts: Vec<&str> = iso_str[..10].split('-').collect();
    if parts.len() == 3 {
        let year = &parts[0][2..];
        let month = parts[1];
        let day = parts[2];
        format!("{}/{}/{}", day, month, year)
    } else {
        "??/??/??".to_string()
    }
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1 << 20 {
        format!("{:.1} MB", bytes as f64 / (1 << 20) as f64)
    } else if bytes >= 1 << 10 {
        format!("{:.1} KB", bytes as f64 / (1 << 10) as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Fetches a manifest by ID and selected game subdomain. Checks disk manifests directory first, then Riot CDN.
pub async fn fetch_manifest(id: u64, project: &str) -> Result<ManifestHl> {
    let id_hex = format!("{:016X}", id);
    let manifests_dir = Path::new("manifests");
    let file_path = manifests_dir.join(format!("{}.manifest", id_hex));

    if !manifests_dir.exists() {
        tokio::fs::create_dir_all(manifests_dir).await?;
    }

    let raw_bytes = if file_path.exists() {
        tracing::info!("Loading manifest {} from disk", id_hex);
        tokio::fs::read(&file_path).await?
    } else {
        tracing::info!("Downloading manifest {} from Riot CDN ({})", id_hex, project);
        let url = format!(
            "https://{}.secure.dyn.riotcdn.net/channels/public/releases/{}.manifest",
            project, id_hex
        );

        let response = reqwest::get(url).await?.error_for_status()?;
        let bytes = response.bytes().await?.to_vec();

        tokio::fs::write(&file_path, &bytes).await?;
        bytes
    };

    let payload = ::hexgate::manhandler::wrapper::extract_payload(&raw_bytes)?;
    let decompressed = zstd::bulk::decompress(payload.compressed_data, payload.uncompressed_size)
        .context("Failed to decompress RMAN payload")?;

    Ok(ManifestHl::Initialized(ManifestWrapper {
        id,
        data: decompressed.into(),
    }))
}

// =========================================================================
// CATALOG DISK AND NETWORK MANAGER
// =========================================================================

pub struct CatalogManager {
    pub local_path: PathBuf,
    pub url: String,
}

impl CatalogManager {
    pub fn new() -> Self {
        Self {
            local_path: PathBuf::from("catalog.json"),
            url: "https://raw.githubusercontent.com/RiotArchiveProject/catalog-download-script/refs/heads/main/catalog.json".to_string(),
        }
    }

    pub fn exists(&self) -> bool {
        self.local_path.exists()
    }

    pub async fn download(&self) -> Result<(), anyhow::Error> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(45))
            .build()?;

        let res = client.get(&self.url)
            .send()
            .await
            .map_err(|e| anyhow!("Failed to request catalog.json: {}", e))?;

        let bytes = res.bytes()
            .await
            .map_err(|e| anyhow!("Failed to read raw bytes: {}", e))?;

        let mut file = std::fs::File::create(&self.local_path)
            .map_err(|e| anyhow!("Failed to create local file: {}", e))?;
        std::io::Write::write_all(&mut file, &bytes)
            .map_err(|e| anyhow!("Failed to write payload to disk: {}", e))?;

        Ok(())
    }

    pub fn load(&self) -> Result<HashMap<String, HashMap<String, CatalogEntry>>, anyhow::Error> {
        let file = std::fs::File::open(&self.local_path)
            .map_err(|e| anyhow!("Failed to open local catalog: {}", e))?;
        let catalog: HashMap<String, HashMap<String, CatalogEntry>> = serde_json::from_reader(file)
            .map_err(|e| anyhow!("Failed to parse catalog JSON: {}", e))?;
        Ok(catalog)
    }
}

// =========================================================================
// TRACING PROGRESS SUBSCRIBER
// =========================================================================

pub struct ProgressBridge {
    pub ui_handle: slint::Weak<AppWindow>,
}

struct MessageVisitor {
    message: String,
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{:?}", value);
        }
    }
}

impl<S> tracing_subscriber::Layer<S> for ProgressBridge
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() == "progress" {
            let mut visitor = MessageVisitor { message: String::new() };
            event.record(&mut visitor);

            // Fix lifetime borrow issues by converting to owned String immediately
            let msg = visitor.message.trim_matches('"').to_string();

            if let Some(pct_start) = msg.find('[') {
                if let Some(pct_end) = msg.find('%') {
                    let percent_str = msg[pct_start + 1..pct_end].trim();
                    let percent_val = percent_str.parse::<f32>().unwrap_or(0.0) / 100.0;

                    let speed_str = if let Some(sp_start) = msg.find("Speed:") {
                        if let Some(sp_end) = msg.find("MB/s") {
                            msg[sp_start + 6..sp_end].trim().to_string()
                        } else { "0.00".to_string() }
                    } else { "0.00".to_string() };

                    let eta_str = if let Some(eta_start) = msg.find("ETA:") {
                        let sub = &msg[eta_start + 4..];
                        if let Some(eta_end) = sub.find('|') {
                            sub[..eta_end].trim().to_string()
                        } else { "--:--".to_string() }
                    } else { "--:--".to_string() };

                    let status_str = msg.clone();
                    let ui_weak = self.ui_handle.clone();

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak.upgrade() {
                            ui.set_extract_percent(percent_val);
                            ui.set_extract_speed(SharedString::from(format!("{} MB/s", speed_str)));
                            ui.set_extract_eta(SharedString::from(eta_str));
                            ui.set_extract_status(SharedString::from(status_str));
                        }
                    });
                }
            }
        }
    }
}

// =========================================================================
// EXPLORER CORE TRAVERSAL HELPERS
// =========================================================================

pub fn flatten_explorer_tree(
    node: &FolderTreeNode,
    depth: usize,
    parent_id: u64,
    flat_list: &mut Vec<ExplorerNode>,
    seen_ids: &mut HashSet<u64>,
) {
    // 1. Prevent duplicate elements from being added
    if node.id != 0 && seen_ids.contains(&node.id) {
        return;
    }

    if node.id != 0 {
        seen_ids.insert(node.id);
    }

    // 2. Flatten empty-named folders to keep the root list clean and duplicate-free
    if node.is_dir && node.name.is_empty() {
        for child in &node.children {
            flatten_explorer_tree(child, depth, parent_id, flat_list, seen_ids);
        }
        return;
    }

    let mut child_ids = Vec::new();
    for child in &node.children {
        child_ids.push(child.id);
    }

    flat_list.push(ExplorerNode {
        id: node.id,
        name: node.name.clone(),
        is_dir: node.is_dir,
        depth,
        size: node.size,
        tag_bitmask: node.tag_bitmask,
        parent_id,
        child_ids,
        is_expanded: false,
        is_selected: false,
        is_visible: true,
    });

    for child in &node.children {
        flatten_explorer_tree(child, depth + 1, node.id, flat_list, seen_ids);
    }
}

pub fn update_explorer_visibility(
    nodes: &mut [ExplorerNode],
    query: &str,
    is_regex: bool,
    selected_tags: &HashSet<u8>,
    select_none_tag: bool,
) {
    let mut id_to_idx: HashMap<u64, usize> = HashMap::new();
    for (idx, node) in nodes.iter().enumerate() {
        id_to_idx.insert(node.id, idx);
    }

    let mut matches_filter: Vec<bool> = vec![false; nodes.len()];

    let regex_opt = if is_regex && !query.is_empty() {
        regex::Regex::new(&format!("(?i){}", query)).ok()
    } else {
        None
    };
    let query_lower = query.to_lowercase();

    for i in 0..nodes.len() {
        let node = &nodes[i];
        if node.is_dir {
            continue;
        }

        let tag_match = if selected_tags.is_empty() && !select_none_tag {
            true
        } else {
            let mask = node.tag_bitmask;
            if mask == 0 {
                select_none_tag
            } else {
                selected_tags.iter().any(|&tag_id| {
                    let bit = tag_id.saturating_sub(1);
                    (mask & (1 << bit)) != 0
                })
            }
        };

        if !tag_match {
            continue;
        }

        let text_match = if query.is_empty() {
            true
        } else if let Some(ref re) = regex_opt {
            re.is_match(&node.name)
        } else {
            node.name.to_lowercase().contains(&query_lower)
        };

        if text_match {
            matches_filter[i] = true;

            let mut curr_parent_id = node.parent_id;
            while curr_parent_id != 0 {
                if let Some(&p_idx) = id_to_idx.get(&curr_parent_id) {
                    matches_filter[p_idx] = true;
                    curr_parent_id = nodes[p_idx].parent_id;
                } else {
                    break;
                }
            }
        }
    }

    let is_filtering = !query.is_empty() || !selected_tags.is_empty() || select_none_tag;

    for i in 0..nodes.len() {
        let node = &nodes[i];

        let matches = if is_filtering {
            matches_filter[i]
        } else {
            true
        };

        if !matches {
            nodes[i].is_visible = false;
            continue;
        }

        let mut parent_expanded = true;
        let mut curr_parent_id = node.parent_id;
        while curr_parent_id != 0 {
            if let Some(&p_idx) = id_to_idx.get(&curr_parent_id) {
                if !nodes[p_idx].is_expanded && !is_filtering {
                    parent_expanded = false;
                    break;
                }
                curr_parent_id = nodes[p_idx].parent_id;
            } else {
                break;
            }
        }

        nodes[i].is_visible = parent_expanded;
    }
}

pub fn select_visible_folder_members(nodes: &mut [ExplorerNode], dir_id: u64, select: bool) {
    let mut targets = HashSet::new();
    targets.insert(dir_id);

    for node in nodes.iter_mut() {
        if targets.contains(&node.parent_id) && node.is_visible {
            targets.insert(node.id);
            node.is_selected = select;
        }
        if node.id == dir_id {
            node.is_selected = select;
        }
    }
}

pub fn select_all_visible_rows(nodes: &mut [ExplorerNode], select: bool) {
    for node in nodes.iter_mut() {
        if node.is_visible {
            node.is_selected = select;
        }
    }
}

// Flat UI explorer representation
#[derive(Clone, Debug)]
pub struct ExplorerNode {
    pub id: u64,
    pub name: String,
    pub is_dir: bool,
    pub depth: usize,
    pub size: u64,
    pub tag_bitmask: u64,
    pub parent_id: u64,
    pub child_ids: Vec<u64>,

    pub is_expanded: bool,
    pub is_selected: bool,
    pub is_visible: bool,
}

// =========================================================================
// 3. MAIN APPLICATION RUNTIME
// =========================================================================

type CacheMap = HashMap<String, GameCache>;

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;

    // Install the Custom Tracing Progress Bridge
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let bridge = ProgressBridge { ui_handle: ui.as_weak() };
    let _ = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(bridge)
        .try_init();

    // Catalog State Holders
    let catalog_manager = Arc::new(CatalogManager::new());
    let catalog_cache: Arc<Mutex<CacheMap>> = Arc::new(Mutex::new(HashMap::new()));

    // Catalog Filtering States
    let active_game = Arc::new(Mutex::new("lol".to_string()));
    let search_query = Arc::new(Mutex::new(String::new()));
    let selected_platforms: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let selected_realms: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let selected_artifacts: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    // Explorer State Holders
    let active_manifest_wrapper: Arc<Mutex<Option<ManifestWrapper>>> = Arc::new(Mutex::new(None));
    let explorer_master_nodes: Arc<Mutex<Vec<ExplorerNode>>> = Arc::new(Mutex::new(Vec::new()));
    let explorer_search = Arc::new(Mutex::new(String::new()));
    let explorer_is_regex = Arc::new(Mutex::new(false));
    let explorer_selected_tags: Arc<Mutex<HashSet<u8>>> = Arc::new(Mutex::new(HashSet::new()));
    let explorer_none_tag = Arc::new(Mutex::new(false));

    // Double-click click state
    struct ClickTracker {
        last_id: String,
        last_time: Instant,
    }
    let click_state = Arc::new(Mutex::new(ClickTracker {
        last_id: String::new(),
        last_time: Instant::now(),
    }));

    // Helper to generate the Pill models for Slint UI (Instantaneous O(1) load from pre-computed values)
    let rebuild_pill_models = {
        let ui_handle = ui.as_weak();
        let cache = catalog_cache.clone();
        let game = active_game.clone();
        let sel_platforms = selected_platforms.clone();
        let sel_realms = selected_realms.clone();
        let sel_artifacts = selected_artifacts.clone();
        move || {
            let Some(ui) = ui_handle.upgrade() else { return; };
            let cache_lock = cache.lock().unwrap();
            let game_lock = game.lock().unwrap();

            let Some(game_data) = cache_lock.get(&*game_lock) else { return; };

            let plat_lock = sel_platforms.lock().unwrap();
            let plat_pills: Vec<SlintFilterPill> = game_data.unique_platforms.iter().map(|name| SlintFilterPill {
                name: SharedString::from(name),
                selected: plat_lock.contains(name),
            }).collect();
            ui.set_platform_pills(ModelRc::from(Rc::new(VecModel::from(plat_pills))));

            let realm_lock = sel_realms.lock().unwrap();
            let realm_pills: Vec<SlintFilterPill> = game_data.unique_realms.iter().map(|name| SlintFilterPill {
                name: SharedString::from(name),
                selected: realm_lock.contains(name),
            }).collect();
            ui.set_realm_pills(ModelRc::from(Rc::new(VecModel::from(realm_pills))));

            let art_lock = sel_artifacts.lock().unwrap();
            let art_pills: Vec<SlintFilterPill> = game_data.unique_artifacts.iter().map(|name| SlintFilterPill {
                name: SharedString::from(name),
                selected: art_lock.contains(name),
            }).collect();
            ui.set_artifact_pills(ModelRc::from(Rc::new(VecModel::from(art_pills))));
            ui.set_show_artifact_pills(!game_data.unique_artifacts.is_empty());
        }
    };

    // Blazing fast UI list refresher (Allocation-free search matches)
    let refresh_slint_list = {
        let ui_handle = ui.as_weak();
        let cache = catalog_cache.clone();
        let game = active_game.clone();
        let search = search_query.clone();
        let sel_platforms = selected_platforms.clone();
        let sel_realms = selected_realms.clone();
        let sel_artifacts = selected_artifacts.clone();
        move || {
            let Some(ui) = ui_handle.upgrade() else { return; };
            let cache_lock = cache.lock().unwrap();
            let game_lock = game.lock().unwrap();
            let search_lock = search.lock().unwrap();
            let plat_lock = sel_platforms.lock().unwrap();
            let realm_lock = sel_realms.lock().unwrap();
            let art_lock = sel_artifacts.lock().unwrap();

            let query = search_lock.to_lowercase();
            let mut slint_items: Vec<SlintManifestItem> = Vec::new();

            if let Some(game_data) = cache_lock.get(&*game_lock) {
                for m in &game_data.manifests {
                    // Check search limit to prevent Slint layout threading overhead
                    if slint_items.len() >= MAX_DISPLAY_ROWS {
                        break;
                    }

                    // 1. Text Search Filter
                    let text_match = query.is_empty()
                        || m.id.to_lowercase().contains(&query)
                        || m.version.to_lowercase().contains(&query)
                        || m.realms.iter().any(|r| r.to_lowercase().contains(&query));
                    if !text_match { continue; }

                    // 2. Platform Multi-Select (OR filter)
                    if !plat_lock.is_empty() {
                        let plat_match = m.platforms.iter().any(|p| plat_lock.contains(p));
                        if !plat_match { continue; }
                    }

                    // 3. Realm Multi-Select (OR filter)
                    if !realm_lock.is_empty() {
                        let realm_match = m.realms.iter().any(|r| realm_lock.contains(r));
                        if !realm_match { continue; }
                    }

                    // 4. Artifact Type Multi-Select (OR filter)
                    if !art_lock.is_empty() {
                        if !art_lock.contains(&m.artifact_type) { continue; }
                    }

                    slint_items.push(SlintManifestItem {
                        id: SharedString::from(&m.id),
                        version_short: SharedString::from(&m.version_short),
                        version_full: SharedString::from(&m.version),
                        realms: SharedString::from(&m.realms_short), // truncated realms list for table view
                        realms_full: SharedString::from(&m.realms.join(", ")), // full list for tooltip hover
                        platforms: SharedString::from(&m.platforms.join(", ")),
                        date_str: SharedString::from(&m.date_str),
                        size_str: SharedString::from(&m.size_str),
                        is_cached: false,
                    });
                }
            }

            let model = Rc::new(VecModel::from(slint_items));
            ui.set_manifest_items(ModelRc::from(model));
        }
    };

    // Refreshes the directory tree nodes layout (Explorer Screen)
    let refresh_explorer_list = {
        let ui_handle = ui.as_weak();
        let master_nodes = explorer_master_nodes.clone();
        move || {
            let Some(ui) = ui_handle.upgrade() else { return; };
            let mut master = master_nodes.lock().unwrap();

            let mut slint_items = Vec::new();
            for node in master.iter_mut() {
                if !node.is_visible {
                    continue;
                }

                slint_items.push(SlintExplorerRow {
                    id: SharedString::from(node.id.to_string()),
                    name: SharedString::from(&node.name),
                    is_dir: node.is_dir,
                    depth: node.depth as i32,
                    is_expanded: node.is_expanded,
                    is_selected: node.is_selected,
                    size_str: SharedString::from(format_size(node.size)),
                });
            }

            let model = Rc::new(VecModel::from(slint_items));
            ui.set_explorer_rows(ModelRc::from(model));
        }
    };

    // Updates visible statuses across directories (Explorer Screen)
    let run_visibility_update = {
        let master_nodes = explorer_master_nodes.clone();
        let search = explorer_search.clone();
        let is_regex = explorer_is_regex.clone();
        let selected_tags = explorer_selected_tags.clone();
        let none_tag = explorer_none_tag.clone();
        let refresher = refresh_explorer_list.clone();
        move || {
            let mut master = master_nodes.lock().unwrap();
            let search_lock = search.lock().unwrap();
            let is_regex_lock = is_regex.lock().unwrap();
            let tags_lock = selected_tags.lock().unwrap();
            let none_lock = none_tag.lock().unwrap();

            update_explorer_visibility(
                &mut master,
                &*search_lock,
                *is_regex_lock,
                &*tags_lock,
                *none_lock,
            );

            drop(master);
            refresher();
        }
    };

    // Sync catalog function
    let sync_catalog_fn = {
        let ui_handle = ui.as_weak();
        let manager = catalog_manager.clone();
        let cache = catalog_cache.clone();
        let reformer_pills = rebuild_pill_models.clone();
        let refresher_list = refresh_slint_list.clone();
        move || {
            let Some(ui) = ui_handle.upgrade() else { return; };
            ui.set_is_syncing_catalog(true);

            let ui_weak = ui_handle.clone();
            let manager = manager.clone();
            let cache = cache.clone();
            let reformer_pills = reformer_pills.clone();
            let refresher_list = refresher_list.clone();

            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                let download_res = rt.block_on(async { manager.download().await });

                match download_res {
                    Ok(_) => {
                        match manager.load() {
                            Ok(parsed_data) => {
                                let ui_weak_inner = ui_weak.clone();
                                let cache_inner = cache.clone();
                                let reformer_inner = reformer_pills.clone();
                                let refresher_inner = refresher_list.clone();

                                let flat_cache = build_manifest_cache(&parsed_data);

                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = ui_weak_inner.upgrade() {
                                        *cache_inner.lock().unwrap() = flat_cache;
                                        ui.set_is_syncing_catalog(false);
                                        reformer_inner();
                                        refresher_inner();
                                    }
                                });
                            }
                            Err(_) => {
                                let ui_weak_inner = ui_weak.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = ui_weak_inner.upgrade() { ui.set_is_syncing_catalog(false); }
                                });
                            }
                        }
                    }
                    Err(_) => {
                        let ui_weak_inner = ui_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak_inner.upgrade() { ui.set_is_syncing_catalog(false); }
                        });
                    }
                }
            });
        }
    };

    // Shared Manifest loader & flattener background task
    let parse_and_open_explorer = {
        let ui_handle = ui.as_weak();
        let active_manifest = active_manifest_wrapper.clone();
        let master_nodes = explorer_master_nodes.clone();
        let active_game_lock = active_game.clone();
        let search = explorer_search.clone();
        let is_regex = explorer_is_regex.clone();
        let tags_set = explorer_selected_tags.clone();
        let none_tag = explorer_none_tag.clone();
        let refresher = refresh_explorer_list.clone();
        move |manifest_id_str: String| {
            let Some(ui) = ui_handle.upgrade() else { return; };
            ui.set_is_syncing_catalog(true); // Show loader while parsing RMAN filesystem

            let ui_weak = ui_handle.clone();
            let active_manifest = active_manifest.clone();
            let master_nodes = master_nodes.clone();
            let project = active_game_lock.lock().unwrap().clone();
            let search = search.clone();
            let is_regex = is_regex.clone();
            let tags_set = tags_set.clone();
            let none_tag = none_tag.clone();
            let refresher = refresher.clone();

            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                let manifest_id_u64 = u64::from_str_radix(&manifest_id_str, 16).unwrap_or(0);

                // Fetch the manifest cleanly using our local fetcher (Using your Zstd and extraction helpers)
                let fetch_res = rt.block_on(async {
                    fetch_manifest(manifest_id_u64, &project).await
                });

                match fetch_res {
                    Ok(ManifestHl::Initialized(wrapper)) => {
                        // 2. Peek files, flatten tree and cache
                        if let Some(tree) = wrapper.peek_fs() {
                            let mut flat_tree = Vec::new();
                            let mut seen_ids = HashSet::new();
                            flatten_explorer_tree(&tree, 0, 0, &mut flat_tree, &mut seen_ids);

                            // Retrieve locale tags inside manifest to build filter pills
                            let mut language_pills = Vec::new();
                            language_pills.push(SlintFilterPill {
                                name: SharedString::from("none"),
                                selected: false,
                            });

                            if let Some(root) = wrapper.root() {
                                if let Some(tags) = root.tags() {
                                    for t in tags {
                                        if let Some(n) = t.name() {
                                            language_pills.push(SlintFilterPill {
                                                name: SharedString::from(n),
                                                selected: false,
                                            });
                                        }
                                    }
                                }
                            }

                            let ui_weak_inner = ui_weak.clone();
                            let active_manifest_inner = active_manifest.clone();
                            let master_nodes_inner = master_nodes.clone();
                            let search_inner = search.clone();
                            let is_regex_inner = is_regex.clone();
                            let tags_inner = tags_set.clone();
                            let none_inner = none_tag.clone();
                            let refresher_inner = refresher.clone();

                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_weak_inner.upgrade() {
                                    *active_manifest_inner.lock().unwrap() = Some(wrapper);
                                    *master_nodes_inner.lock().unwrap() = flat_tree;

                                    // Reset explorer states
                                    *search_inner.lock().unwrap() = String::new();
                                    *is_regex_inner.lock().unwrap() = false;
                                    tags_inner.lock().unwrap().clear();
                                    *none_inner.lock().unwrap() = false;

                                    ui.set_language_pills(ModelRc::from(Rc::new(VecModel::from(language_pills))));
                                    ui.set_is_regex_active(false);
                                    ui.set_is_syncing_catalog(false);
                                    ui.set_current_screen(SharedString::from("explorer")); // Swap screen!

                                    refresher_inner();
                                }
                            });
                        }
                    }
                    Ok(ManifestHl::Uninitialized(_)) => {
                        eprintln!("Manifest loaded as uninitialized state");
                        let ui_weak_inner = ui_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak_inner.upgrade() { ui.set_is_syncing_catalog(false); }
                        });
                    }
                    Err(e) => {
                        eprintln!("Failed to fetch manifest: {}", e);
                        let ui_weak_inner = ui_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak_inner.upgrade() { ui.set_is_syncing_catalog(false); }
                        });
                    }
                }
            });
        }
    };

    // =========================================================================
    // SCREEN 1 (CATALOG SELECTION) EVENT REGISTRATIONS
    // =========================================================================

    ui.on_game_selected({
        let ui_handle = ui.as_weak();
        let game = active_game.clone();
        let plat_sel = selected_platforms.clone();
        let realm_sel = selected_realms.clone();
        let art_sel = selected_artifacts.clone();
        let reformer_pills = rebuild_pill_models.clone();
        let refresher_list = refresh_slint_list.clone();
        move |game_key| {
            let Some(ui) = ui_handle.upgrade() else { return; };
            *game.lock().unwrap() = game_key.to_string();

            plat_sel.lock().unwrap().clear();
            realm_sel.lock().unwrap().clear();
            art_sel.lock().unwrap().clear();

            ui.set_active_game(game_key);
            ui.set_has_selection(false);

            reformer_pills();
            refresher_list();
        }
    });

    ui.on_search_changed({
        let search = search_query.clone();
        let refresher = refresh_slint_list.clone();
        move |text| {
            *search.lock().unwrap() = text.to_string();
            refresher();
        }
    });

    ui.on_sync_catalog({
        let sync = sync_catalog_fn.clone();
        move || { sync(); }
    });

    ui.on_platform_pill_clicked({
        let sel = selected_platforms.clone();
        let reformer_pills = rebuild_pill_models.clone();
        let refresher_list = refresh_slint_list.clone();
        move |name| {
            let mut list = sel.lock().unwrap();
            if list.contains(name.as_str()) { list.remove(name.as_str()); } else { list.insert(name.to_string()); }
            drop(list); reformer_pills(); refresher_list();
        }
    });

    ui.on_realm_pill_clicked({
        let sel = selected_realms.clone();
        let reformer_pills = rebuild_pill_models.clone();
        let refresher_list = refresh_slint_list.clone();
        move |name| {
            let mut list = sel.lock().unwrap();
            if list.contains(name.as_str()) { list.remove(name.as_str()); } else { list.insert(name.to_string()); }
            drop(list); reformer_pills(); refresher_list();
        }
    });

    ui.on_artifact_pill_clicked({
        let sel = selected_artifacts.clone();
        let reformer_pills = rebuild_pill_models.clone();
        let refresher_list = refresh_slint_list.clone();
        move |name| {
            let mut list = sel.lock().unwrap();
            if list.contains(name.as_str()) { list.remove(name.as_str()); } else { list.insert(name.to_string()); }
            drop(list); reformer_pills(); refresher_list();
        }
    });

    ui.on_manifest_row_clicked({
        let ui_handle = ui.as_weak();
        let state = click_state.clone();
        let cache = catalog_cache.clone();
        let game = active_game.clone();
        let explore_trigger = parse_and_open_explorer.clone();
        move |id| {
            let Some(ui) = ui_handle.upgrade() else { return; };
            let mut tracker = state.lock().unwrap();
            let now = Instant::now();

            if tracker.last_id == id.as_str() && now.duration_since(tracker.last_time) < Duration::from_millis(300) {
                explore_trigger(id.to_string());
                return;
            }

            tracker.last_id = id.to_string();
            tracker.last_time = now;

            ui.set_active_manifest_id(id.clone());

            // Read the cached item to populate the detail preview
            let cache_lock = cache.lock().unwrap();
            let game_lock = game.lock().unwrap();
            if let Some(game_data) = cache_lock.get(&*game_lock) {
                if let Some(m) = game_data.manifests.iter().find(|i| i.id == id.as_str()) {
                    ui.set_selected_manifest(SlintManifestItem {
                        id: id.clone(),
                        version_short: SharedString::from(""),
                        version_full: SharedString::from(&m.version),
                        realms: SharedString::from(&m.realms.join(", ")),
                        realms_full: SharedString::from(&m.realms.join(", ")),
                        platforms: SharedString::from(&m.platforms.join(", ")),
                        date_str: SharedString::from(&m.date_str),
                        size_str: SharedString::from(&m.size_str),
                        is_cached: false,
                    });
                    ui.set_has_selection(true);
                }
            }
        }
    });

    ui.on_manifest_row_double_clicked({
        let explore_trigger = parse_and_open_explorer.clone();
        move |id| {
            explore_trigger(id.to_string());
        }
    });

    // =========================================================================
    // SCREEN 2 (ASSET EXPLORER) EVENT REGISTRATIONS
    // =========================================================================

    // Back button
    ui.on_back_to_selection({
        let ui_handle = ui.as_weak();
        move || {
            let Some(ui) = ui_handle.upgrade() else { return; };
            ui.set_current_screen(SharedString::from("selection"));
        }
    });

    // Real-time explorer search edited
    ui.on_explorer_search_changed({
        let search = explorer_search.clone();
        let updater = run_visibility_update.clone();
        move |text| {
            *search.lock().unwrap() = text.to_string();
            updater();
        }
    });

    // Toggle Regex Search Mode Button click
    ui.on_toggle_regex_search({
        let ui_handle = ui.as_weak();
        let is_regex = explorer_is_regex.clone();
        let updater = run_visibility_update.clone();
        move || {
            let Some(ui) = ui_handle.upgrade() else { return; };
            let mut reg = is_regex.lock().unwrap();
            *reg = !*reg;
            ui.set_is_regex_active(*reg);
            drop(reg);
            updater();
        }
    });

    // Language locale pill click handler (includes "none" tag filtering)
    ui.on_language_pill_clicked({
        let ui_handle = ui.as_weak();
        let active_manifest = active_manifest_wrapper.clone();
        let sel_tags = explorer_selected_tags.clone();
        let none_tag = explorer_none_tag.clone();
        let updater = run_visibility_update.clone();
        move |name| {
            let Some(ui) = ui_handle.upgrade() else { return; };
            let manifest_lock = active_manifest.lock().unwrap();
            let Some(ref wrapper) = *manifest_lock else { return; };
            let Some(root) = wrapper.root() else { return; };

            let mut tags_lock = sel_tags.lock().unwrap();
            let mut none_lock = none_tag.lock().unwrap();

            if name == "none" {
                *none_lock = !*none_lock;
            } else if let Some(tags) = root.tags() {
                // Find tag ID corresponding to name
                if let Some(target_tag) = tags.iter().find(|t| t.name().unwrap_or("") == name.as_str()) {
                    let tid = target_tag.id();
                    if tags_lock.contains(&tid) {
                        tags_lock.remove(&tid);
                    } else {
                        tags_lock.insert(tid);
                    }
                }
            }

            // Sync Slint pill rendering statuses
            let mut language_pills = Vec::new();
            language_pills.push(SlintFilterPill {
                name: SharedString::from("none"),
                selected: *none_lock,
            });

            if let Some(tags) = root.tags() {
                for t in tags {
                    if let Some(n) = t.name() {
                        language_pills.push(SlintFilterPill {
                            name: SharedString::from(n),
                            selected: tags_lock.contains(&t.id()),
                        });
                    }
                }
            }

            ui.set_language_pills(ModelRc::from(Rc::new(VecModel::from(language_pills))));

            drop(tags_lock);
            drop(none_lock);
            updater();
        }
    });

    // Toggle folder rows expansion callback
    ui.on_toggle_folder_expansion({
        let master_nodes = explorer_master_nodes.clone();
        let updater = run_visibility_update.clone();
        move |node_id_str| {
            let node_id = node_id_str.parse::<u64>().unwrap_or(0);
            let mut master = master_nodes.lock().unwrap();
            if let Some(node) = master.iter_mut().find(|n| n.id == node_id) {
                node.is_expanded = !node.is_expanded;
            }
            drop(master);
            updater();
        }
    });

    // Toggle row checkbox selection callback (includes folder recursive selections)
    ui.on_toggle_row_selection({
        let master_nodes = explorer_master_nodes.clone();
        let refresher = refresh_explorer_list.clone();
        move |node_id_str, selected| {
            let node_id = node_id_str.parse::<u64>().unwrap_or(0);
            let mut master = master_nodes.lock().unwrap();

            // Find row target
            let mut is_dir = false;
            if let Some(node) = master.iter().find(|n| n.id == node_id) {
                is_dir = node.is_dir;
            }

            if is_dir {
                // If it is a folder row, recursively select all its visible children!
                select_visible_folder_members(&mut master, node_id, selected);
            } else if let Some(node) = master.iter_mut().find(|n| n.id == node_id) {
                node.is_selected = selected;
            }

            drop(master);
            refresher();
        }
    });

    // Handle "Select All" / "Deselect All" button clicks
    ui.on_toggle_select_all({
        let master_nodes = explorer_master_nodes.clone();
        let refresher = refresh_explorer_list.clone();
        move |select| {
            let mut master = master_nodes.lock().unwrap();
            select_all_visible_rows(&mut master, select);
            drop(master);
            refresher();
        }
    });

    // Handle Extraction Button Click
    ui.on_extract_selected({
        let ui_handle = ui.as_weak();
        let active_manifest = active_manifest_wrapper.clone();
        let master_nodes = explorer_master_nodes.clone();
        let active_game_lock = active_game.clone();
        move || {
            let Some(ui) = ui_handle.upgrade() else { return; };
            let master = master_nodes.lock().unwrap();

            // 1. Gather all file IDs that are selected
            let selected_file_ids: Vec<u64> = master
                .iter()
                .filter(|n| !n.is_dir && n.is_selected)
                .map(|n| n.id)
                .collect();

            if selected_file_ids.is_empty() {
                println!("No files selected for extraction.");
                return;
            }

            // 2. Prompt user to pick output directory using rfd File Dialog
            if let Some(target_folder) = rfd::FileDialog::new().pick_folder() {
                ui.set_is_extracting(true); // Toggle extraction progress bar visible
                ui.set_extract_percent(0.0);
                ui.set_extract_speed(SharedString::from("0.0 MB/s"));
                ui.set_extract_eta(SharedString::from("--:--"));
                ui.set_extract_status(SharedString::from("Preparing download plan..."));

                let ui_weak = ui_handle.clone();
                let manifest_lock = active_manifest.lock().unwrap();
                let Some(ref wrapper) = *manifest_lock else { return; };
                let game = active_game_lock.lock().unwrap().clone();

                // Create a completely new Install struct specifically for the target path
                // (Bypasses local SQLite database updates completely)
                let extraction_install = Install::new(
                    "ExtractionTemp".to_string(),
                    LeagueVersion::from_parts(0, 0, 0), // unused
                    target_folder,
                    PathBuf::from("temp.db"), // unused
                    ManifestHl::Initialized(wrapper.clone()),
                    Vec::new(),
                );

                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();

                    // Static official CDNs based on subdomain
                    let base_url = format!("https://{}.secure.dyn.riotcdn.net/channels/public", game);
                    let bundle_sources = vec![WBundleSource::new(base_url)];

                    // Satisfies the internal check by constructing a chunk source from the extraction target itself
                    let self_source = InstallationChunkSource::new_from_db(Arc::new(extraction_install.clone()));
                    let chunk_sources = vec![self_source];

                    let res = rt.block_on(async {
                        extraction_install.download_files(&selected_file_ids, chunk_sources, bundle_sources).await
                    });

                    let ui_weak_inner = ui_weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak_inner.upgrade() {
                            ui.set_is_extracting(false);
                            match res {
                                Ok(_) => println!("Extraction finished successfully!"),
                                Err(e) => eprintln!("Extraction failed: {}", e),
                            }
                        }
                    });
                });
            }
        }
    });

    // =========================================================================
    // STARTUP LOAD SEQUENCE (Fully Thread-Decoupled)
    // =========================================================================
    if catalog_manager.exists() {
        let ui_weak = ui.as_weak();
        let manager = catalog_manager.clone();
        let cache = catalog_cache.clone();
        let reformer_pills = rebuild_pill_models.clone();
        let refresher_list = refresh_slint_list.clone();

        ui.set_is_syncing_catalog(true);

        std::thread::spawn(move || {
            match manager.load() {
                Ok(parsed) => {
                    let flat_cache = build_manifest_cache(&parsed);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak.upgrade() {
                            *cache.lock().unwrap() = flat_cache;
                            ui.set_is_syncing_catalog(false);
                            reformer_pills();
                            refresher_list();
                        }
                    });
                }
                Err(_) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        sync_catalog_fn();
                    });
                }
            }
        });
    } else {
        sync_catalog_fn();
    }

    ui.run()
}