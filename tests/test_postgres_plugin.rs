#![allow(clippy::integer_arithmetic)]

/// Integration testing for the PostgreSQL plugin
/// This requires a PostgreSQL database named 'solana' be setup at localhost at port 5432
/// This is automatically setup in the CI environment.
/// To setup manually on Ubuntu Linux, do the following,
/// sh -c 'echo "deb http://apt.postgresql.org/pub/repos/apt $(lsb_release -cs)-pgdg main" > /etc/apt/sources.list.d/pgdg.list'
/// wget --quiet -O - https://www.postgresql.org/media/keys/ACCC4CF8.asc | apt-key add -
/// apt install -y postgresql-14
/// sudo /etc/init.d/postgresql start
///
/// sudo -u postgres psql --command "CREATE USER solana WITH SUPERUSER PASSWORD 'solana';"
/// sudo -u postgres createdb -O solana solana
/// PGPASSWORD=solana psql -U solana -p 5432 -h localhost -w -d solana -f scripts/create_schema.sql
/// Before run "cargo test", do a build by "cargo build" otherwise it may use stale build of the dynamic library.
/// The test will cover transmitting accounts, transaction and slot and
/// block metadata.
use {
    libloading::Library,
    log::*,
    serde_json::json,
    serial_test::serial,
    solana_accountsdb_plugin_postgres::{
        accountsdb_plugin_postgres::AccountsDbPluginPostgresConfig,
        postgres_client::SimplePostgresClient,
    },
    solana_core::validator::ValidatorConfig,
    solana_local_cluster::{
        local_cluster::{ClusterConfig, LocalCluster},
        validator_configs::*,
    },
    solana_rpc::rpc::JsonRpcConfig,
    solana_runtime::snapshot_config::SnapshotConfig,
    solana_sdk::{clock::Slot, epoch_schedule::MINIMUM_SLOTS_PER_EPOCH},
    solana_streamer::socket::SocketAddrSpace,
    std::{
        fs::{self, File},
        io::Read,
        io::Write,
        path::{Path, PathBuf},
    },
    tempfile::TempDir,
};

const RUST_LOG_FILTER: &str =
    "info,solana_core::replay_stage=warn,solana_local_cluster=info,local_cluster=info";

fn farf_dir() -> PathBuf {
    let dir: String = std::env::var("FARF_DIR").unwrap_or_else(|_| "farf".to_string());
    fs::create_dir_all(dir.clone()).unwrap();
    PathBuf::from(dir)
}

fn generate_account_paths(num_account_paths: usize) -> (Vec<TempDir>, Vec<PathBuf>) {
    let account_storage_dirs: Vec<TempDir> = (0..num_account_paths)
        .map(|_| tempfile::tempdir_in(farf_dir()).unwrap())
        .collect();
    let account_storage_paths: Vec<_> = account_storage_dirs
        .iter()
        .map(|a| a.path().to_path_buf())
        .collect();
    (account_storage_dirs, account_storage_paths)
}

fn generate_accountsdb_plugin_config() -> (TempDir, PathBuf) {
    let tmp_dir = tempfile::tempdir_in(farf_dir()).unwrap();
    let mut path = tmp_dir.path().to_path_buf();
    path.push("accounts_db_plugin.json");
    let mut config_file = File::create(path.clone()).unwrap();

    let library_name = "libsolana_accountsdb_plugin_postgres.so";
    // Convert it to a Path and get the parent directory
    let current_dir: PathBuf = Path::new(path.to_str().unwrap())
        .parent() // Get the parent directory (one level up)
        .and_then(|p| p.parent()) // Two levels up
        .and_then(|p| p.parent()) // 3 levels up
        .unwrap() // Handle the case where there is no parent (for example, at the root)
        .to_path_buf();

    let mode = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };

    let target_debug_path = current_dir.join("target").join(mode);

    info!("The target debug path: {target_debug_path:?}");

    let mut config_content = json!({
        "libpath": target_debug_path.join(library_name).to_str().unwrap(),
        "connection_str": "host=localhost user=solana password=solana port=5432",
        "threads": 20,
        "batch_size": 20,
        "panic_on_db_errors": true,
        "accounts_selector" : {
            "accounts" : ["*"]
        },
        "transaction_selector" : {
            "mentions" : ["*"]
        }
    });

    if std::env::consts::OS == "macos" {
        let library_name = "libsolana_accountsdb_plugin_postgres.dylib";
        config_content["libpath"] = json!(target_debug_path.join(library_name).to_str().unwrap());
    }

    write!(config_file, "{}", config_content.to_string()).unwrap();
    (tmp_dir, path)
}

#[allow(dead_code)]
struct SnapshotValidatorConfig {
    snapshot_dir: TempDir,
    snapshot_archives_dir: TempDir,
    account_storage_dirs: Vec<TempDir>,
    validator_config: ValidatorConfig,
    plugin_config_dir: TempDir,
}

fn setup_snapshot_validator_config(
    snapshot_interval_slots: u64,
    num_account_paths: usize,
) -> SnapshotValidatorConfig {
    // Create the snapshot config
    let bank_snapshots_dir = tempfile::tempdir_in(farf_dir()).unwrap();
    let snapshot_archives_dir = tempfile::tempdir_in(farf_dir()).unwrap();
    let snapshot_config = SnapshotConfig {
        full_snapshot_archive_interval_slots: snapshot_interval_slots,
        incremental_snapshot_archive_interval_slots: Slot::MAX,
        full_snapshot_archives_dir: snapshot_archives_dir.path().to_path_buf(),
        incremental_snapshot_archives_dir: snapshot_archives_dir.path().to_path_buf(),
        bank_snapshots_dir: bank_snapshots_dir.path().to_path_buf(),
        ..SnapshotConfig::default()
    };

    // Create the account paths
    let (account_storage_dirs, account_storage_paths) = generate_account_paths(num_account_paths);

    let (plugin_config_dir, path) = generate_accountsdb_plugin_config();

    let on_start_geyser_plugin_config_files = Some(vec![path]);

    let mut rpc_config = JsonRpcConfig::default();
    rpc_config.full_api = true;

    // Create the validator config
    let validator_config = ValidatorConfig {
        snapshot_config: snapshot_config,
        account_paths: account_storage_paths,
        accounts_hash_interval_slots: snapshot_interval_slots,
        on_start_geyser_plugin_config_files,
        enforce_ulimit_nofile: false,
        rpc_config,
        ..ValidatorConfig::default()
    };

    SnapshotValidatorConfig {
        snapshot_dir: bank_snapshots_dir,
        snapshot_archives_dir,
        account_storage_dirs,
        validator_config,
        plugin_config_dir,
    }
}

fn test_local_cluster_start_and_exit_with_config(socket_addr_space: SocketAddrSpace) {
    const NUM_NODES: usize = 1;
    let config = ValidatorConfig {
        enforce_ulimit_nofile: false,
        ..ValidatorConfig::default()
    };
    let mut config = ClusterConfig {
        validator_configs: make_identical_validator_configs(&config, NUM_NODES),
        node_stakes: vec![3; NUM_NODES],
        cluster_lamports: 100,
        ticks_per_slot: 8,
        slots_per_epoch: MINIMUM_SLOTS_PER_EPOCH as u64,
        stakers_slot_offset: MINIMUM_SLOTS_PER_EPOCH as u64,
        ..ClusterConfig::default()
    };
    info!("starting cluster....");
    let cluster = LocalCluster::new(&mut config, socket_addr_space);
    assert_eq!(cluster.validators.len(), NUM_NODES);
}

#[test]
#[serial]
fn test_postgres_plugin() {
    solana_logger::setup_with_default(RUST_LOG_FILTER);

    unsafe {
        let filename = match std::env::consts::OS {
            "macos" => "libsolana_accountsdb_plugin_postgres.dylib",
            _ => "libsolana_accountsdb_plugin_postgres.so",
        };

        let lib = Library::new(filename);
        if lib.is_err() {
            info!("Failed to load the dynamic library {} {:?}", filename, lib);
            return;
        }
    }

    info!("Starting local cluster and exit");

    //test_local_cluster();

    info!("Starting my local cluster and exit");

    let socket_addr_space = SocketAddrSpace::new(true);
    test_local_cluster_start_and_exit_with_config(socket_addr_space);

    info!("Setup cluster with snapshot");

    // First set up the cluster with 1 node
    let snapshot_interval_slots = 50;
    let num_account_paths = 3;

    let leader_snapshot_test_config =
        setup_snapshot_validator_config(snapshot_interval_slots, num_account_paths);

    let mut file = File::open(
        &leader_snapshot_test_config
            .validator_config
            .on_start_geyser_plugin_config_files
            .as_ref()
            .unwrap()[0],
    )
    .unwrap();
    let mut contents = String::new();
    file.read_to_string(&mut contents).unwrap();
    let plugin_config: AccountsDbPluginPostgresConfig = serde_json::from_str(&contents).unwrap();

    let result = SimplePostgresClient::connect_to_db(&plugin_config);
    if result.is_err() {
        info!("Failed to connecto the PostgreSQL database. Please setup the database to run the integration tests. {:?}", result.err());
        return;
    }

    let stake = 10_000;
    let mut config = ClusterConfig {
        node_stakes: vec![stake],
        cluster_lamports: 1_000_000,
        validator_configs: make_identical_validator_configs(
            &leader_snapshot_test_config.validator_config,
            1,
        ),
        ..ClusterConfig::default()
    };

    let cluster = LocalCluster::new(&mut config, socket_addr_space);

    assert_eq!(cluster.validators.len(), 1);
    let contact_info = &cluster.entry_point_info;

    info!("Contact info: {:?}", contact_info);

    // Get slot after which this was generated
    let snapshot_archives_dir = &leader_snapshot_test_config
        .validator_config
        .snapshot_config
        .full_snapshot_archives_dir;
    info!("Waiting for snapshot");

    let snap_info = cluster.wait_for_next_full_snapshot(snapshot_archives_dir, None);
    info!("Found: full snapshot {:?}", snap_info);
}
