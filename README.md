# picusbase CLI

CLI để scaffold, build, deploy và quản lý Compute Functions trên nền tảng Picus.

## Cài đặt

```bash
curl -sSf https://raw.githubusercontent.com/picusbase/picusbase-cli/main/install.sh | sh
```

Script tự phát hiện OS/arch và tải binary phù hợp (`macos-aarch64`, `macos-x86_64`, `linux-x86_64`, `linux-aarch64`).

**Build từ source** (cần Rust toolchain):

```bash
cargo install --git https://github.com/picusbase/picusbase-cli.git picusbase
```

Kiểm tra và cập nhật lên phiên bản mới nhất:

```bash
picusbase --version
picusbase upgrade
```

## Cấu hình

Đăng nhập bằng personal access token (PAT `pbpat_...`, tạo tại Console → System Admin → Access Tokens). `--default-url` là URL Console gốc:

```bash
picusbase config set \
  --profile default \
  --default-url http://console.picusbase.localhost \
  --default-project-ref <project_ref> \
  --personal-token <pbpat_...>

picusbase config use default
picusbase config show
picusbase function list
```

Có PAT, CLI đổi lấy access token và đi qua console-api. Không có PAT, `--default-url` là Compute Control Plane (`http://localhost:4080/api`, cần port-forward). Config file được ghi quyền 0600.

Thứ tự ưu tiên resolve: flag CLI → biến môi trường `PICUS_*` → config file → default.

## Workflow phổ biến

### 1. Scaffold function mới

```bash
picusbase init functions <domain> <function-id> --language javascript --name "My Function"
```

Ba lane hỗ trợ:

| Flag | Lane | File chính |
|---|---|---|
| (bỏ trống) hoặc `--language rust` | Rust/Wasm | `src/lib.rs` |
| `--language javascript` | JavaScript | `src/index.mjs` |
| `--language python` | Python | `src/handler.py` |

### Function Rust (native-v1)

Cần `picus.toml`, `Cargo.toml` và `src/bin/native_worker.rs`. Build chạy trong Docker (Linux) nên cần Docker đang chạy. Dependency từ crates.io không cần gì thêm. Dependency kiểu `path` (kể cả bắc cầu) và workspace cha được mount vào container, nên phải cùng nằm dưới một thư mục (ví dụ `sdk/` cạnh `functions/` của bundle). Kết quả build nằm ở `functions/target/`. Đặt `PICUS_NATIVE_BUILD_PLATFORM` (vd `linux/amd64`) khi nền tảng đích khác kiến trúc máy build.

### 2. Build

```bash
picusbase function build --path . --release
```

### 3. Deploy

```bash
picusbase deploy functions --rebuild
```

Deploy nhiều function từ thư mục cha:

```bash
picusbase deploy functions func-a func-b
```

Upload thủ công artifact đã build:

```bash
picusbase function upload \
  --id my-function \
  --file ./artifact.tar.gz \
  --runtime-class javascript-v1
```

### 3b. Deploy cả bundle (`deploy schema`)

```bash
picusbase deploy schema --path <bundle> --project-ref <ref>
```

Role, entity, function, automation và AI agent của bundle. Chạy lại được.

| Cờ | Tác dụng |
|---|---|
| `--dry-run` (`--plan`) | Không ghi gì: kiểm tra bundle, liệt kê cái nào mới / đã có |
| `--keep-going` | Một function lỗi thì vẫn deploy các function còn lại, báo mọi lỗi ở cuối |
| `--skip-functions` | Bỏ qua `functions/` |
| `--no-publish` | Chỉ tạo bản nháp entity |

Function Rust được kiểm tra trước khi ghi gì (`native_worker.rs`, path dependency trong `Cargo.toml`, Docker). Bundle có function không build được thì dừng khi chưa deploy gì.

### 4. Chạy nhanh (smoke test)

```bash
picusbase \
  --executor-url http://localhost:4396 \
  --project-ref <project_ref> \
  --user-id smoke \
  --user-role authenticated \
  function run --id <function-id> --params '{"key":"value"}'
```

### 5. Route binding

`deploy` tự tạo route từ `openapi.yaml`. Thêm/xoá thủ công:

```bash
picusbase route list
picusbase route bind --function <function-id> --path /orders/{id} --method GET --auth-mode authenticated
picusbase route delete --id <binding-id>
```

### 6. Jobs

```bash
picusbase job create --function-id <function-id> --params '{"key":"value"}'
picusbase job list
picusbase job delete --id <job-id>
```

### 7. Schedules (chưa hỗ trợ khi dùng PAT)

```bash
picusbase schedule create \
  --name "Daily sync" \
  --cron "0 0 * * *" \
  --function-id <function-id>

picusbase schedule list
picusbase schedule delete --id <schedule-id>
```

## Biến môi trường

| Biến | Mô tả |
|---|---|
| `PICUS_URL` | Base URL của API |
| `PICUS_PROJECT_REF` | Project ref mặc định |
| `PICUS_EXECUTOR_URL` | URL của HTTP worker |
| `PICUS_PERSONAL_TOKEN` | Personal access token |
| `PICUS_AUTH_TOKEN` | Bearer token cho `function run --executor-url` |
| `PICUS_USER_ID` | User ID (dev bypass) |
| `PICUS_PROFILE` | Profile được chọn |

## Examples

Thư mục `examples/` chứa sample function minh họa guest ABI:

- `hello-world/` — Rust function đơn giản
- `calculator/` — parse input, trả kết quả phép tính
- `count_error_logs/` — dùng `host::query_read_only()`
