use clap::{Parser, Subcommand, Args};
use anyhow::Result;

#[derive(Parser)]
#[command(name = "seekfs-ctl", version)]
struct Cli {
    /// Daemon address
    #[arg(long, default_value = "http://127.0.0.1:7070")]
    addr: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check daemon health
    Health,
    /// Generate and publish a manifest and HEAD for a file in a namespace
    GenManifest(GenManifestArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Health => health(&cli.addr).await?,
        Command::GenManifest(args) => gen_manifest(args).await?,
    }
    Ok(())
}

async fn health(addr: &str) -> anyhow::Result<()> {
    let url = format!("{}/healthz", addr.trim_end_matches('/'));
    let txt = reqwest::Client::new().get(&url).send().await?.text().await?;
    println!("{}", txt);
    Ok(())
}

#[derive(Args, Debug, Clone)]
struct GenManifestArgs {
    /// Path to seekfs.toml for S3 config
    #[arg(long, default_value = "seekfs.toml")]
    config: String,
    /// Namespace
    #[arg(long)]
    ns: String,
    /// File path within namespace (relative path under files/)
    #[arg(long)]
    path: String,
    /// Slab size in MiB
    #[arg(long, default_value_t = 8u32)]
    slab_size_mib: u32,
    /// Explicit version; if omitted, increments from HEAD or uses 1 if no HEAD
    #[arg(long)]
    version: Option<u64>,
}

#[derive(Debug, serde::Deserialize)]
struct ToolConfig { s3: Option<S3Cfg> }

#[derive(Debug, serde::Deserialize, Clone)]
struct S3Cfg {
    endpoint: Option<String>,
    region: String,
    bucket: String,
    access_key_id: Option<String>,
    secret_access_key: Option<String>,
}

async fn gen_manifest(args: GenManifestArgs) -> Result<()> {
    use aws_config::BehaviorVersion;
    use aws_sdk_s3::config::{Credentials, Region, SharedCredentialsProvider};
    use aws_sdk_s3::{Client, Config};
    use crc32c::crc32c;
    use flate2::{write::GzEncoder, Compression};
    use sha2::{Digest, Sha256};
    use std::time::{SystemTime, UNIX_EPOCH};

    // Load config
    let s = std::fs::read_to_string(&args.config)?;
    let cfg: ToolConfig = toml::from_str(&s)?;
    let s3 = cfg.s3.ok_or_else(|| anyhow::anyhow!("[s3] missing in config"))?;

    // Build SDK client
    let mut b = Config::builder().behavior_version(BehaviorVersion::latest()).region(Region::new(s3.region.clone()));
    if let Some(ep) = &s3.endpoint { b = b.endpoint_url(ep).force_path_style(true); }
    if let (Some(ak), Some(sk)) = (s3.access_key_id.clone(), s3.secret_access_key.clone()) {
        b = b.credentials_provider(SharedCredentialsProvider::new(Credentials::new(ak, sk, None, None, "seekfs-tools")));
    }
    let client = Client::from_conf(b.build());

    // Determine version
    let head_key = seekfs_core::manifest_head_key(&args.ns);
    let version = match args.version {
        Some(v) => v,
        None => {
            match client.get_object().bucket(&s3.bucket).key(&head_key).range("bytes=0-64").send().await {
                Ok(o) => {
                    let body = o.body.collect().await?;
                    let bytes = body.into_bytes();
                    let s = String::from_utf8_lossy(&bytes);
                    parse_head_version(&s).unwrap_or(0) + 1
                }
                Err(_) => 1,
            }
        }
    };

    // HEAD the file and stream contents to compute hash and CRCs
    let file_key = format!("namespaces/{}/files/{}", args.ns, args.path);
    let meta = client.head_object().bucket(&s3.bucket).key(&file_key).send().await?;
    let size = meta.content_length().unwrap_or(0) as u64;

    let slab = (args.slab_size_mib as u64) * 1024 * 1024;
    let mut page_table = Vec::new();
    let mut sha = Sha256::new();
    let mut page_id: u32 = 0;
    let mut off = 0u64;
    while off < size {
        let end = (off + slab).min(size);
        let range = format!("bytes={}-{}", off, end - 1);
        let o = client.get_object().bucket(&s3.bucket).key(&file_key).range(range).send().await?;
        let mut stream = o.body;
        let mut page_buf: Vec<u8> = Vec::with_capacity((end - off) as usize);
        while let Some(chunk) = stream.try_next().await? { sha.update(&chunk); page_buf.extend_from_slice(&chunk); }
        let crc = crc32c(&page_buf);
        page_table.push(seekfs_core::PageInfo { page_id, off, len: (end - off) as u32, crc32c: crc });
        page_id += 1;
        off = end;
    }

    let hash_hex = format!("sha256:{}", hex::encode(sha.finalize()));
    let fe = seekfs_core::FileEntry {
        path: args.path.clone(),
        size,
        hash: hash_hex.clone(),
        page_table,
        storage: seekfs_core::manifest::ObjectLoc { key: file_key.clone(), etag: meta.e_tag().map(|s| s.to_string()), sse: None },
    };
    let manifest = seekfs_core::Manifest { version, files: vec![fe], parents: if version > 1 { vec![version - 1] } else { vec![] }, tombstones: vec![], created_at: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() };

    let json = serde_json::to_vec(&manifest)?;
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    use std::io::Write;
    enc.write_all(&json)?;
    let gz = enc.finish()?;

    let man_key = seekfs_core::manifest_version_key(&args.ns, version);
    client.put_object().bucket(&s3.bucket).key(&man_key).body(gz.into()).send().await?;
    // Write HEAD
    client.put_object().bucket(&s3.bucket).key(&head_key).body(format!("v-{}\n", version).into_bytes().into()).send().await?;
    println!("Wrote {} and {}", head_key, man_key);
    Ok(())
}

fn parse_head_version(s: &str) -> Option<u64> {
    let line = s.lines().next()?.trim();
    if let Some(t) = line.strip_prefix("v-") { return t.parse::<u64>().ok(); }
    if let Some(t) = line.strip_prefix('v') { return t.parse::<u64>().ok(); }
    line.parse::<u64>().ok()
}
