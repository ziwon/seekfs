// S3 key layout helpers according to README spec
// Layout:
// s3://bucket/
// └─ namespaces/{ns}/
//    ├─ HEAD
//    ├─ manifests/v-{n}.json.gz
//    └─ files/{path...}/v-{h}/...

pub fn manifest_head_key(ns: &str) -> String {
    format!("namespaces/{}/HEAD", ns)
}

pub fn manifest_version_key(ns: &str, version: u64) -> String {
    format!("namespaces/{}/manifests/v-{}.json.gz", ns, version)
}

pub fn file_key(ns: &str, path: &str, hash: Option<&str>) -> String {
    match hash {
        Some(h) if !h.is_empty() => format!("namespaces/{}/files/{}/v-{}", ns, path, h),
        _ => format!("namespaces/{}/files/{}", ns, path),
    }
}

