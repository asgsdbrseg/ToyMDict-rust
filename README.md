# ToyMDict（Rust + Tauri 重构版）

用 [raymanzhang/mdx](https://github.com/raymanzhang/mdx)（libmdx）替换原 Python `readmdict.py`，用 Tauri 2 替换 pywebview + 本地 HTTP 服务。前端 `dist/` 全部编译进二进制，**运行时只有一个 exe，无 `_internal`、无安装器、免安装**。

## 运行

把这三个文件放在同一个目录：

```
ToyMDict.exe          <- 单文件主程序
custom.css            <- 手动放置，自定义字体/样式
variants.json         <- 手动放置，异体字表
```

首次运行会在同目录自动生成 `dict_groups.json`（词典列表与分组）和 `toymdict.log`（**每次启动清空，只记录本次**）。缺 `custom.css` / `variants.json` 时程序仍可启动，只是没有自定义字体和异体字功能。

> `dict_groups.json` 与 Python 版**同名、同结构**（`all_dicts` / `groups` / `excluded` / `current_group`），可直接把 Python 版的配置文件复制过来复用，无需重新导入词典。`groups` 里分组的排列顺序按文件中的顺序原样显示并原样写回，不会按分组名重排。只读取 `dict_groups.json`，不再兼容旧版 `toymdict.json`。**不要同时运行 Python 版和 Rust 版**，两者会写同一份文件。

> Windows 10 1803+ / 11 自带 WebView2 Runtime，无需额外安装。极老的精简系统若提示缺少 WebView2，装一次「Microsoft Edge WebView2 Runtime」即可（这是系统组件，不是本程序的安装步骤）。

> 需要调试界面时按 **F12** 或 **Ctrl+Shift+I** 打开 DevTools；**启动程序不会自动打开**。完整快捷键见下方「快捷键」节。

## 功能对齐

| 功能 | 实现 |
|---|---|
| 导入 MDX（文件 / 文件夹递归 3 层） | 系统文件对话框，自动关联同名 MDD |
| 词典分组、排序、排除、恢复 | `dict_groups.json` 持久化（与 Python 版共用）；**分组顺序按 JSON 原样保留，不重排** |
| 词条前缀搜索 + 异体字 | 多词典 rayon 并行，同名条目合并；**精确匹配置顶**，其余按词条长度升序（短词在前）；不限制结果条数。异体字用「首字前缀 + 字符类正则过滤」两步法（对齐 Python 版），无组合上限；前缀查找改用**全量字节级扫描**，结果与 Python 版一致 |
| 词典信息（分组界面双击） | 与 Python 版一致：文件路径、标题（含来源）、**描述按 `<iframe srcdoc>` 渲染**、技术参数 6 项、全部 Header 字段折叠区 |
| 错误提示 | 配置读写、词典加载/搜索、词条加载、导入跳过、分组删除等失败均弹窗 |
| 输入框自动填充 | 关闭 WebView2 general autofill，不再弹出「保存的信息」建议 |
| 高级搜索 | 正则（默认忽略大小写）/ 异体字 / 条目 / 全文 / 多词典勾选 |
| 流式结果 | 每本词典独立线程，每 30 条推送一次，扫完立即更新状态，不等待、不排序、不合并 |
| 切换分组自动搜索 | 切换分组后用当前搜索框关键词立即重搜新分组（搜索框为空时不触发） |
| 全文标签透明匹配 | `<u>南朝</u><u>宋</u>` 可命中「南朝宋」；命中在词条正文内跨标签高亮 |
| 词条正文与 MDD 资源 | `mdx://` 自定义协议，CSS / 图片 / 音频 / 字体按需读取 |
| 图片查看器、ESC 关闭 | iframe 内 ESC 经 postMessage 转发到主窗口 |
| 系统级全局快捷键 | 双击 Ctrl 取词：外部窗口模拟 Ctrl+C（带扫描码）→ 还原剪贴板 → 置前搜索；**应用内选词**直接读前端选区，不动剪贴板 |
| 应用内全局快捷键 | F12 / Ctrl+Shift+I 打开 DevTools、Ctrl+F 聚焦搜索、Ctrl+Shift+F 高级搜索等 |
| DevTools | 编译期启用 `devtools`，**启动不自动打开**，仅由快捷键手动打开 |
| 全局字体兜底 | `custom.css` 由 exe 旁文件经协议提供，改完按 F5 刷新（或重启）生效 |

## 快捷键

| 快捷键 | 作用 |
|---|---|
| **F12** / **Ctrl+Shift+I** | 打开开发者工具 DevTools |
| **双击 Ctrl**（系统级） | 在任意软件（含 ToyMDict 自己的界面 / 词条 iframe）里选中文字后快速按两次 Ctrl，自动取词并搜索 |
| **Ctrl+Shift+H** | 开 / 关「双击 Ctrl 取词」 |
| **Ctrl+F** | 聚焦主界面搜索框并全选 |
| **Ctrl+Shift+F** | 打开高级搜索 |
| **Ctrl+O** / **Ctrl+Shift+O** | 导入词典文件 / 文件夹 |
| **Ctrl+G** | 打开词典分组管理 |
| **F5** / **Ctrl+R** | 重新载入界面（`custom.css` 改完按此生效） |
| **Esc** | 关闭弹窗 / 图片查看器 |
| **Ctrl+/** | 显示快捷键帮助浮层 |

**DevTools 说明**：`src-tauri/Cargo.toml` 给 `tauri` 启用了 `devtools` feature，因此 release 版也具备打开 DevTools 的能力（等价于 Python 版 `webview.start(debug=True)`）。但后端 `setup()` 里**没有**调用 `open_devtools()`，只有用户按下上表快捷键时前端才调用 `open_devtools` 命令，所以**启动应用不会自动弹出 DevTools**（等价于 Python 版 `OPEN_DEVTOOLS_IN_DEBUG = False`）。DevTools 是独立的 WebView2 窗口；Windows 上 Tauri 的 `close_devtools()` 不受支持，关闭请直接点 DevTools 窗口右上角 ×。

**双击 Ctrl 取词说明**：用 `WH_KEYBOARD_LL` 低级键盘钩子实现，不依赖额外插件或随附 DLL。要点：

- 400ms 内两次「按下-松开」Ctrl 才算一次手势；按住不放产生的系统自动重复不会误触发；
- **统一走剪贴板**：与 Python 版 `global_hotkey.py` 一致，无论焦点在哪个窗口都模拟 Ctrl+C 并读写系统剪贴板。之前「前台窗口属于本进程」时短路到 `captureInAppSelection`，那条前端选区通道在真实 WebView2 里没生效，正是「应用内取词无效、但同时启动 Python 版就正常」的原因——Python 版始终走剪贴板，把本程序窗口当外部窗口一样复制。只有应用内且剪贴板确实没被更新时，才回退到前端选区通道；
- **前端本地双击 Ctrl 通道**：主文档自己监听 400ms 内两次 Ctrl 并直接读选区；焦点在词条 iframe 内时，由词条页注入的脚本自行判定手势并把选区 postMessage 回主窗口（主窗口收不到 iframe 的 keydown）。两条通道用 800ms 时间戳去重，不会把同一个词搜索两次；
- 模拟按键带 `MapVirtualKeyW` 查出的扫描码，Chromium 类应用也能识别；模拟的 Ctrl+C 带 `LLKHF_INJECTED` 标志，被钩子自身忽略，不会造成回环触发；
- 钩子加固：`SetWindowsHookExW` 的 hmod 传 NULL（与 pynput / rdev 一致；传 EXE 模块句柄在部分 Windows 版本上会让全局钩子静默失效），安装线程的消息循环不再因 `GetMessageW` 返回 -1 就退出并卸载钩子；
- 取词后还原用户原有剪贴板文本（原剪贴板不是纯文本时不覆盖，避免破坏图片/文件）；
- 只取首行、最多 200 字符，避免误复制整篇文章塞进搜索框；
- 后端结果走 `externalSearchFromHotkey()`（带去重），前端本地通道直接调 `externalSearch()`，与 Python 版行为一致；应用内不置前窗口（置前会清掉 WebView2 选区），只有从外部窗口取词后才把窗口带到前台；
- 仅 Windows 有实现；其它平台 `hotkey::start()` 返回 false，程序照常运行，应用内快捷键仍可用。

## 交互细节（本版）

- **分组顺序**：`groups` 改用 `IndexMap`。读取 `dict_groups.json` 时保留分组的写入顺序，保存时原序写回；删除分组用 `shift_remove`（`IndexMap::remove` 是 `swap_remove`，会打乱其余顺序）。下拉框与分组管理界面都按这个顺序显示。
- **搜索结果**：精确匹配（忽略大小写）置顶，其余按词条字符数升序（同长按字典序）。后端仍会定位精确匹配并自动展示内容。**结果条数不再限制**：每本词典的相似前缀由 libmdx 全量返回（其内部会把 `max_count` 钳制到剩余词条数，传 `u64::MAX` 不会无限循环），最终列表也不再 `truncate(300)`；高级搜索同样取消了每词典 10000 条命中的硬上限，仍可用「停止」随时中断。
- **普通搜索的异体字（首字 + 正则两步法）**：对齐 Python 版 `MdxWrapper.search`，不再生成「每个字的异体字」笛卡尔积，因此**取消了 128 个组合上限**。做法是逐字符构造「原字 + 异体字」字符类正则（如 `黄钟` → `^[黄黃][钟鍾鐘]`，非汉字用 `regex::escape` 按字面量处理、整体忽略大小写），**只用首字**的候选写法做前缀搜索，再用该正则过滤取回的词条。候选数恒等于首字异体数、与关键词长度无关，20 字长词也不会漏匹配；关键词没有任何字符可展开时自动回退为普通前缀搜索。高级搜索的异体字仍走 `expand_pattern()` 字符类展开，不受影响。
- **前缀搜索结果完整性（对齐 Python 版 `search_prefix`）**：`dict.rs::scan_prefixes()` 分批遍历全部索引，逐条做零分配的字节级 `starts_with`（只折叠 ASCII 大小写，等价 Python 的 `bytes.lower()`），中间不匹配的词条直接跳过、不终止；多个异体字候选一次扫描覆盖，天然去重。**不再使用 libmdx 的 `get_similar_indexes`**：该接口以「首个命中词条」的 sort key 为比较基准（而不是用户输入的搜索词），且循环里遇到第一个不相似就 `break`，隐含「同前缀词条在索引中严格连续」的假设。中文词典常按拼音 / ICU collation 排序，这一假设不成立，于是搜「子」只返回开头 3 条，而 Python 版的 `search_prefix` 扫描所有 key block 能取全。**这与结果条数上限无关**——上限早已取消，漏结果来自那个提前 `break`。代价是普通搜索需顺序读一遍索引（4096 条一批、缓存友好），超大词典首次搜索会比旧版略慢。
- **切换分组自动搜索**：`tauri-bridge.js` 的 api 代理包装了 `switch_group`，等后端写完 `current_group` 后再调用前端 `triggerSearch()`，用当前搜索框关键词重新搜索新分组；搜索框为空时 `triggerSearch()` 自身不做事。主界面与分组管理界面的两个下拉框都自动生效，且没有改动上传的 5 个前端基线文件。
- **词典信息**：在「词典分组」界面双击词典名，弹窗的 HTML 结构、字段、措辞与样式**逐字对齐 Python 版 `get_dict_info`**：文件路径；标题（带「来自 Header / 文件名」标记，异常 Title 回退文件名）；描述用固定的 `<iframe srcdoc="...">` 渲染，`srcdoc` 值经 `html.escape(quote=True)` 属性转义——浏览器解析属性时把 `&lt;br&gt;` 解回 `<br>`，iframe 内再把 `<font>` / `<br>` / `<hr>` / `<img>` 等当作真正的 HTML 标签渲染，同时与主窗口样式隔离。注意 Header 是 XML，标签只能写成 `&lt;br&gt;` 这类实体，而 libmdx 的 `DbInfo::from_xml` 直接取属性原始字节、不做实体解码；因此 `dict_metadata()` 优先用 `parse_header_attrs()`（内部已 `xml_unescape`）取 Description，回退到 `db.description` 时再补一次 `xml_unescape`，保证拿到的是「可渲染的原始 HTML 文本」。属性转义统一走 `html::escape_html_attribute()`（等价 Python `html.escape(quote=True)`），整段描述 HTML 由 `html::dict_info_description_html()` 生成，不做额外清理，行为与 Python 版一致。技术参数固定 6 项（MDX版本、编码、加密、创建日期、词条数千分位、MDD 文件数，空值自动隐藏）；「全部 Header 字段」折叠区直接取自 MDX/ZDB Header XML 的原始属性（长值截断 300）。为拿到原始字段，`dict_metadata()` 从 libmdx 的 `MetaUnit::raw_header_xml` 解析根元素属性，而不只用已解析的 `db_info`。词典打不开时显示「词典文件未加载」灰字 + ⚠ 错误，并额外弹出错误提示。
- **错误弹窗**：后端 `report_error()` 覆盖配置解析与保存、异体字表解析、词典加载与搜索、词条加载、词典信息读取、导入跳过、失效路径清理、分组删空等分支；前端桥接层每 1.5 秒拉取一次待展示错误并弹窗（`get_pending_errors` 会清空队列，不与启动期拉取重复；后端只在首次拉取时刷新 UI，轮询不会重建分组下拉框）。前端自身的 `error` / `unhandledrejection` 也仍会弹窗。
- **输入框**：`tauri.conf.json` 设 `generalAutofillEnabled: false`，并给所有 `<input>` 加 `autocomplete="off"`，消除 WebView2 在文本框弹出的「保存的信息」自动填充建议。
- **应用内取词（双通道 + 去重）**：`tauri-bridge.js` 在主文档捕获阶段监听双击 Ctrl，直接读主界面输入框 / 主文档 / 同源 iframe 的选区（取首行限 200 字后交给 `externalSearch()`）；焦点在词条 iframe 内时主窗口收不到 keydown，因此 `build_entry_html()` 注入的回传脚本也会自行判定双击 Ctrl，主动 postMessage `toymdict-hotkey-selection` 上报选区。后端 `hotkey.rs` 同时统一走「模拟 Ctrl+C + 剪贴板」，结果推给 `externalSearchFromHotkey()`；`window.__lastLocalCapture` 时间戳保证 800ms 内只搜索一次。只有应用内剪贴板没变化时后端才回退到 `captureInAppSelection`，用 `toymdict-get-selection` / `toymdict-selection` 广播向 iframe 要选区（500ms 超时自动清理）。**词条 iframe 走自定义 `mdx` 协议、与主窗口不同源**，`contentDocument` / `getSelection()` 都读不到，postMessage 是必需的。取词逻辑集中在桥接层、词条 HTML 骨架与 `hotkey.rs`，未改动上传的 5 个前端基线文件。

## 词典 CSS 与资源解析

词条页用 `<base href="{mdx 协议}/mdd/{词典}/">` 把 `<link href="xxx.css">`、`<img src="...">` 等相对路径回落到 `mdx://` 协议。资源解析对齐 Python 版，分两级：

1. **MDD 内查找**：把请求路径显式展开成「原样 / 小写 × 正斜杠 / 反斜杠 × 有无前导分隔符」共 10 个候选逐个尝试。V2 词典 libmdx 会把 `/` 转成 `\`，V3 用 ICU collation，斜杠方向、前导分隔符与大小写的组合并不总能命中。
2. **外挂文件回退**：很多词典把样式表作为独立 `.css` 放在 `.mdx` 同目录，并不打进 MDD。此时按相对路径在词典目录下精确查找，再逐段大小写不敏感匹配（例如正文写 `css/style.css`、磁盘上是 `css/STYLE.CSS`）。

`.css` 一律返回 `text/css; charset=utf-8`：浏览器对样式表做严格 MIME 检查，返回 `application/octet-stream` 会被直接拒绝应用，而图片不受此限制——这正是「MDD 图片能显示、词典 CSS 不生效」的典型表现。资源路径同时拒绝 `..`、绝对路径与 Windows 盘符/UNC，避免越界读取。

## 构建

### Windows 本机（推荐）

需要 Rust 1.85+（edition 2024）与 Windows SDK / MSVC 生成工具。

```cmd
build.bat
```

产出 `ToyMDict.exe`。`tauri.conf.json` 里 `bundle.active = false`、`frontendDist = "../dist"`，所以 `cargo build --release` 直接产出内嵌前端的单文件 exe，不会生成 MSI / NSIS 安装包，也不会有附属资源目录。

### GitHub Actions

`.github/workflows/build.yml` 在 `windows-latest` 上编译并把 `ToyMDict.exe + custom.css + variants.json` 作为 artifact 上传。适合在没有本地 Rust 环境时取 exe。

### Linux 交叉编译（本次交付的 exe 就是这样产出的）

```bash
rustup target add x86_64-pc-windows-gnu
# Debian/Ubuntu: apt install gcc-mingw-w64-x86-64 g++-mingw-w64-x86-64 clang lld llvm
./build-cross.sh
```

不需要 Windows SDK、不需要 MSVC、不需要 Wine。

### 手动命令

```cmd
cargo build --release --manifest-path src-tauri\Cargo.toml
copy target\release\toymdict.exe ToyMDict.exe
```

## 单文件是怎么做到的

- 前端 `dist/`（HTML/CSS/JS）通过 Tauri 的 `frontendDist` 编译进二进制，运行时从内存提供，**没有 `_internal` 或 web/ 目录**。
- `bundle.active = false`，不生成 MSI / NSIS 安装包，`cargo build --release` 直接就是可运行的 exe。
- `webview2-com-sys` 上游只对 MSVC 静态链接 `WebView2LoaderStatic.lib`，GNU 目标会退化成依赖外部 `WebView2Loader.dll`。本工程用 `[patch.crates-io]` 打了补丁（`vendor/webview2-com-sys`）：
  - **MSVC 构建**：保持上游静态链接行为；
  - **windows-gnu 构建**：把 `WebView2Loader.dll`（WebView2 SDK 允许再分发的引导库）编进 exe，首次调用时释放到临时目录并用 `LoadLibraryW` / `GetProcAddress` 解析 5 个导出函数。
  - 因此 exe 的导入表只有 `kernel32 / user32 / ole32 / msvcrt / advapi32` 等系统 DLL，**不依赖任何随附 DLL**。
- 两个上游小修正：`vendor/mdx/src/utils/io_utils.rs` 的 `fix_windows_path` 在 Windows 分支给不可变参数赋值（上游编译错误）；`vendor/mdx` 关闭了过时的 doctest，并显式启用纯 Rust 的 `icu` feature（上游要求必须启用 `icu` 或 `rust-icu` 之一）。

## 前端 ↔ 后端 IPC 契约

`dist/tauri-bridge.js` 把 `pywebview.api.xxx(...)` 的位置参数还原成具名参数再调用 `invoke`。这里有两个 Tauri 2 特有的坑：

1. **参数键名**：`#[tauri::command]` 宏默认把 Rust 的 `snake_case` 参数转成 `camelCase` 作为 JS 键名（例如 `dict_id` → `dictId`、`use_variants` → `useVariants`）。桥接层因此**同时发送 snake_case 与 camelCase 两种键**；Tauri 用 `payload.get(key)` 逐个取值，多余键被忽略，双写对版本变化安全。只发一种会在调用 `search` / `add_dict_to_group` / `get_dict_info` 等命令时报 `missing required key dictId`。
2. **对话框 ACL**：页面里的原生 `confirm()` 会被 Tauri 转成 `plugin:dialog|message` 命令。`capabilities/default.json` 必须包含 `dialog:default`（内含 `allow-message` / `allow-open` / `allow-save`），否则删除分组等操作会报 `Command plugin:dialog|message not allowed by ACL`。
3. **启动时序**：`event.listen('js_call')` 需要一次 IPC 往返才注册完成，后端在 `get_pending_errors` / `setup` 里推送的启动期 `updateUI` 可能在此之前发出而丢失，导致分组下拉框空白。桥接层在监听注册完成后派发一次 `pywebviewready`，让前端重新拉取、后端重发 UI。
4. **前端基线替换**：`dist/` 的 `index.html` / `app.js` / `style.css` / `entry.js` / `entry.css` 以 Python 版为基线（含「高级搜索默认全不选」等行为），但必须保留的适配有三处——`index.html` 用相对路径引 `style.css` / `app.js` 且先加载 `tauri-bridge.js`（`custom.css` 是 exe 旁的外部文件，由桥接层经协议动态注入，不能写成静态 `<link>`）；`app.js` 的 `isTrustedMessage` 放行 `null` 与本地 `*.localhost` 来源，否则词条 iframe 的 `resize` / `esc` / `open-image` 消息会被全部丢弃，图片查看器与 ESC 关闭都会失效；`app.js` / `entry.js` 的**高度自适应兜底**（见下条），否则词条会偶发卡在 0 高度。
5. **词条 iframe 高度自适应死锁**：iframe 以 `height:0px` 起步，靠 `entry.js` 测量 `#mdx-content` 后 postMessage `resize` 撑开。Chromium 会把**零面积的跨域 iframe 按「不可见帧」节流甚至停发 BeginFrame**，rAF / ResizeObserver 回调可能不再派发——一旦首条上报恰好在内容就绪前测得 0 或丢失，就形成「0 高 → 零面积 → rAF 停发 → 永不上报」的死锁（表现为 iframe `995×0`、内容已加载但不显示，偶发）。修复分三层：`entry.js` 的 `schedule()` 在 rAF 之外加 80ms `setTimeout` 兜底（被节流的帧里定时器仍会派发）并保留 `load` + 启动 2.5 秒内定时重测；`app.js` 在 iframe `load` 后分 4 波补发 `calcHeight`（0/200/800/2000ms）；`setContent` 后 2.5 秒的看门狗对仍为 0 高的已展开 iframe 再补一轮。任一通道生效即恢复，`measure` 幂等去重，重复触发无副作用。
6. **全局快捷键与 DevTools**：应用内快捷键集中在 `dist/tauri-bridge.js`，不改动上面 5 个基线文件。监听注册在捕获阶段，只拦截明确列出的组合，`Esc` 等原有逻辑照常放行。`open_devtools` 是自定义命令，不需要额外 ACL；启动流程不调用它，只有快捷键会触发。

改动命令签名或桥接层后运行契约检查，它会比对 Rust 参数、桥接键名与前端实参数量：

```bash
python3 scripts/check_api_contract.py    # 静态契约检查
python3 scripts/verify_frontend_ipc.py   # 无头 Chromium 严格 IPC 模拟（需系统有 chromium）
```

## 验证记录

- `cargo check` / `cargo check --target x86_64-pc-windows-gnu` / `cargo build --target x86_64-pc-windows-gnu --release`：通过（含 Win32 钩子、剪贴板与窗口置前代码，无警告）。
- Rust 单元测试 **44 项**（主 crate 与 `vendor/mdx` 均零警告）：异体字展开（高级搜索字符类 + 普通搜索首字前缀/正则两步法）、前缀搜索（字节级判断对齐 `bytes.lower()`、模拟「不连续匹配」验证不再提前中断、扫描同时套用异体字正则）、正则编译、标签透明匹配、HTML 路径重写、词条骨架生成、词典信息描述（属性转义对齐 Python `html.escape`、srcdoc 解码还原、iframe 结构、单层转义不二次解码）、资源路径归一化 / 安全校验 / MDD 候选 / CSS MIME / 外挂文件大小写不敏感回退；全局快捷键 6 项（双击判定的间隔 / 超时 / 触发后重新计时 / 时钟回拨、取词开关、文本清洗）；分组 JSON 顺序往返与删除不重排；排序（精确匹配置顶、忽略大小写、长度升序）；原始 Header 属性解析。
- **内存占用优化**：此前常驻内存偏高的根源不是数据本身，而是三层缓存的叠加——①搜索走 rayon 并行，`thread_local!` 的 reader 缓存每个工作线程独立保留 8 个 `MdxReader`（16 核机器最多 128 个 reader 常驻）；②每个 reader 的内容块 LRU 缓存 10 个解压 record block（单块可达数 MB）+ 16 个解压 key block，MDD 又各一份；③批量扫描结束后这些块一直留在缓存里。现在：每线程 reader 缓存降到 **2**、内容块 LRU 降到 **4**、key block LRU 降到 **4**，并在普通搜索 / 高级搜索的每本词典扫描结束后调用 `MdxReader::trim_caches()` 立即释放全部数据块缓存（后续按需读取只是重新解压，少量时间换内存）。WebView2 侧通过 `additionalBrowserArgs` 追加 `--disk-cache-size=33554432` 与 `--enable-low-end-device-mode`（磁盘缓存限 32 MB、低内存设备模式；若在你的机器上渲染变卡可删掉后者）。残余占用主要是 WebView2 渲染进程本身（约 80–120 MB），属浏览器内核基线，应用侧无法再压。
- `scripts/check_api_contract.py`：22 个命令的参数键与前端调用逐项一致（本版未新增命令，契约保持不变）。
- 无头 Chromium 严格 IPC 模拟：按真实 Tauri 规则校验 camelCase 键与 ACL，触发全部 22 个命令、7 组快捷键（F12 / Ctrl+Shift+I / Ctrl+F / Ctrl+Shift+F / Ctrl+G / Ctrl+/ / Ctrl+Shift+H）及 `js_call` 推送（含 `captureInAppSelection`），0 错误；F12 与 Ctrl+Shift+I 各自命中 `open_devtools`；验证「切换分组后自动触发搜索」的调用顺序（`switch_group` 之后出现 `search`），构造真实选区后「应用内选词」把「南朝宋」写进搜索框并触发搜索，以及模拟跨域词条 iframe 的 `postMessage` 选区回传；新增「词典信息描述 iframe srcdoc 渲染」端到端检查，在真实 Chromium 里确认 srcdoc 属性解码回原始 `<font>` 标签、`<br>` 被渲染成元素、正文文本正常显示。此前做过「回退修复」的对照实验，能精确复现 `missing required key useVariants` / `dictId` 与 `not allowed by ACL` 两类报错。
- DevTools 启动行为：`setup()` 中不调用 `open_devtools()`；release 构建启用 `tauri/devtools` feature，命令只由快捷键触发，启动不弹 DevTools。
- 词典信息：`dict_metadata()` 从 libmdx 的 `raw_header_xml` 解析原始 Header 属性（保留键名与顺序），标题来源判断与技术参数 6 项对齐 Python 版；弹窗 HTML 结构逐项比对 `window_api.get_dict_info`。词典打不开时降级为「词典文件未加载」+ ⚠ 提示并报错。
- 错误通道：`get_pending_errors` 只在首次调用时 `refresh_ui`，前端 1.5 秒轮询不会重建分组下拉框；`tauri.conf.json` 的 `generalAutofillEnabled: false` 已通过 Tauri 配置解析。
- 无头 Chromium 前端测试：桥接层参数映射、js_call 事件分派、结果流式追加、iframe 渲染、消息来源校验、custom.css 注入、高级搜索面板、应用内选词选区读取。
- PE 检查：`PE32+ GUI x86-64`、导入表无第三方 DLL、前端资源字符串内嵌。
- 词典 CSS 端到端：构造「正文引用 `css/style.css`、磁盘为 `css/STYLE.CSS`、无 MDD」的场景，验证返回内容与 `text/css; charset=utf-8`；反斜杠路径命中、`../` 越界被拒绝。
- 配置复用：直接反序列化 Python 版 `dict_groups.json`（缺 `name` / 缺字段 / `current_group` 为空串）并补齐不变量。
- brotli 解压核对 exe 实际内嵌的 6 个前端资源，确认是替换后且带 Tauri 适配的新版（构建目录里残留的旧缓存未编入）。

> 注意：exe 在 Linux 沙箱中无法真正启动 GUI（没有 Windows / WebView2 Runtime）。功能逻辑通过上述单元测试与前端测试验证，界面请在 Windows 上运行确认。

## 目录结构

```
Cargo.toml                  workspace + [patch.crates-io]（release 用 thin-LTO + strip）
build.bat / build-cross.sh  Windows 本机 / Linux 交叉编译脚本
scripts/check_api_contract.py  命令签名 / 桥接参数 / 前端调用一致性检查
scripts/verify_frontend_ipc.py 无头 Chromium 严格 IPC 模拟
dist/                       前端，编译进 exe
  index.html / app.js / style.css
  entry.html / entry.js / entry.css
  tauri-bridge.js           pywebview.api -> invoke 兼容层 + js_call 分派 + 全局快捷键 + 应用内选词 + DevTools 触发
src-tauri/
  src/main.rs               Tauri 入口、commands、mdx:// 协议、词典信息弹窗
  src/dict.rs               词典读取、普通/高级搜索、异体字（首字+正则 / 字符类展开）
  src/hotkey.rs             系统级全局快捷键（双击 Ctrl 取词：应用内选区通道 + 外部剪贴板通道）
  src/state.rs              配置、日志、状态
  src/html.rs               HTML 重写、标签透明化、词条 iframe 骨架、描述 srcdoc 转义
  tauri.conf.json           单文件配置
  capabilities/             Tauri 2 权限
vendor/mdx/                 raymanzhang/mdx v0.5.0 源码（AGPL-3.0）
vendor/webview2-com-sys/     单文件补丁：gnu 目标内嵌并动态加载 WebView2Loader
custom.css / variants.json  运行时外部文件
```

## 与 Python 版的差异（已知）

- **正则语法**：改用 Rust `regex`，不支持 Python `re` 的环视 `(?=...)`、`(?<=...)` 与反向引用 `\1`；这类表达式会给出明确的「正则表达式无效」提示。其余常用语法一致，且同样默认忽略大小写。
- **全文搜索**：为实时扫描（无 Lucene/Tantivy 索引），大词典首次全文搜索会较慢；本版已取消每词典 10000 条命中上限，可随时「停止」中断。libmdx 自带 Tantivy FTS，若词典旁有 `.idx` 索引可在后续版本接入。
- **排序**：`mdx` 启用纯 Rust 的 `icu` feature（ICU4X），前缀查找与相似词定位由 libmdx 的 locale-aware sort key 完成；主界面结果列表**精确匹配置顶**，其余按**词条长度升序**（同长按字典序），不再按 Unicode 码位序。分组顺序则按 `dict_groups.json` 的写入顺序原样保留。
- **配置**：改用与 Python 版同名的 `dict_groups.json`，字段结构一致，可直接复用；读取时给 `AppConfig` / `DictMeta` 加了 `#[serde(default)]`，Python 版写出的条目即使缺 `name` 等字段也能正常加载。只读取 `dict_groups.json`，不再兼容旧版 `toymdict.json`。
- **全局热键**：已移植原版「双击 Ctrl 取词」（见「快捷键」节）。原实现依赖 `pynput` + `pyperclip`，Rust 版改用 Win32 低级键盘钩子、系统剪贴板 API 与带扫描码的模拟按键，不需要额外插件，也不增加随附 DLL。取词链路与 Python 版一致（一律模拟 Ctrl+C 读剪贴板），并额外补了前端本地双击 Ctrl + 词条 iframe 选区主动上报通道，用时间戳与后端结果去重，修好在程序自己界面里选词取不到的问题。其它平台暂不提供系统级取词。
- **词典信息描述**：与 Python 版一致，用 `<iframe srcdoc>` + `html.escape(quote=True)` 属性转义隔离显示（srcdoc 属性会把实体再解一层，标签才得以渲染），固定高度 150px、带边框圆角。两者的差别只在数据来源：Python 版由 `readmdict.py` 解析 Header XML 时解码实体，本版由 `dict_metadata()` 的 `parse_header_attrs()`（`xml_unescape`）解码，拿到的都是可渲染的原始 HTML 文本。
- **词典加密**：`Encrypted=2`、V3 UUID 派生密钥等格式由 libmdx 处理，行为以其实现为准。

## 许可证

`vendor/mdx` 来自 raymanzhang/mdx，采用 **AGPL-3.0**（见 `LICENSE-AGPL-3.0.txt`）。本工程整体同样按 AGPL-3.0 分发；若闭源使用，需替换为自有解析器或取得相应授权。
