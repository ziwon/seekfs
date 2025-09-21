set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

alias b := build

default: build

build:
    cargo build --workspace

run-sdk:
    SEEKFS_CONFIG=seekfs.toml RUST_LOG=info cargo run -p seekfs-daemon

run-crt:
    SEEKFS_CONFIG=seekfs.toml RUST_LOG=info cargo run -p seekfs-daemon --features crt

run-crt-slab:
    SEEKFS_CONFIG=seekfs.toml RUST_LOG=info cargo run -p seekfs-daemon --features "crt,slabfile"

ctl-health:
    cargo run -p seekfs-tools -- health

fmt:
    cargo fmt --all

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

kind-up:
    kind create cluster --name seekfs --config deploy/kind/kind-cluster.yaml

kind-down:
    kind delete cluster --name seekfs

minio-apply:
    helm repo add minio https://charts.min.io/
    helm repo update
    helm upgrade --install minio minio/minio \
      -n object --create-namespace \
      -f deploy/minio/values-ephemeral.yaml \
      --set rootUser=minioadmin,rootPassword=minioadmin

minio-apply-pv:
    helm repo add minio https://charts.min.io/
    helm repo update
    helm upgrade --install minio minio/minio \
      -n object --create-namespace \
      -f deploy/minio/values-pv.yaml \
      --set rootUser=minioadmin,rootPassword=minioadmin

mc-alias:
    mc alias set minio http://localhost:9000 minioadmin minioadmin --api s3v4
    mc alias list | sed -n '1,200p'

mc-mb bucket="seekfs": 
    mc mb minio/{{bucket}} || true
    mc ls minio

mc-upload file key="warm.bin" bucket="seekfs":
    mc cp {{file}} minio/{{bucket}}/{{key}}
    mc ls minio/{{bucket}}

mc-upload-ns file path="warm.bin" ns="dev" bucket="seekfs":
    mc cp {{file}} minio/{{bucket}}/namespaces/{{ns}}/files/{{path}}
    mc ls minio/{{bucket}}/namespaces/{{ns}}/files || true

smoke-head key="" ns="dev" path="warm.bin" hash="":
    if [ -n "{{key}}" ]; then qs="key={{key}}"; else qs="ns={{ns}}&path={{path}}"; fi
    if [ -n "{{hash}}" ]; then qs="$qs&hash={{hash}}"; fi
    curl -sS "http://127.0.0.1:7070/test_head?$qs" | sed -e 's/%$//'

smoke-stream key="" len="8388608" off="0" ns="dev" path="warm.bin" hash="sha256-dev" page_id="0":
    qs="off={{off}}&len={{len}}&ns={{ns}}&path={{path}}&hash={{hash}}&page_id={{page_id}}"
    if [ -n "{{key}}" ]; then qs="key={{key}}&$qs"; fi
    curl -sS "http://127.0.0.1:7070/test_stream?$qs" | sed -e 's/%$//'

smoke-demo ns="dev":
    dd if=/dev/urandom of=/tmp/warm.bin bs=1M count=8 status=none
    just mc-alias
    just mc-mb
    just mc-upload-ns /tmp/warm.bin warm.bin {{ns}}
    just smoke-head '' {{ns}} warm.bin
    just smoke-stream '' 8388608 0 {{ns}} warm.bin

manifest-gen ns path slab_size_mib="8" version="":
    ver="{{version}}"; \
    if [ -n "$ver" ]; then \
      cargo run -p seekfs-tools -- gen-manifest --ns {{ns}} --path {{path}} --slab-size-mib {{slab_size_mib}} --version "$ver"; \
    else \
      cargo run -p seekfs-tools -- gen-manifest --ns {{ns}} --path {{path}} --slab-size-mib {{slab_size_mib}}; \
    fi

minio-uninstall:
    helm uninstall minio -n object || true

minio-port-forward:
    kubectl -n object port-forward svc/minio 9000:9000 &
    kubectl -n object port-forward svc/minio-console 9001:9001 &
