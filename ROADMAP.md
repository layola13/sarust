# sarust 开发计划 ROADMAP（2026-10-29 修订：Cargo 提前）

目标：支持所有 Rust 特性。节奏：每簇定向验证 + 全量扫表零回退 + 及时 commit + push（2026-10-29 用户改令，不再只留本地）。
方向问 Jev；低置信度先只读诊断/复核波及面再动手。sci/sla 及其他插件仓库只读，改动只落在 sarust。
新特性先 loud 标记再实现，绝不静默吞。

## Phase 0：基线重建（新容器，正在进行）
- sa/sla 按 sala restore-colab.sh 恢复并冒烟（sa --version + sla hello）
- Rust 工具链：nightly-2026-09-28 + rustc-dev/rust-src/llvm-tools
- sars 改名重做：rsc_driver → sars（build.sh 尊重 RUSTUP_TOOLCHAIN；运行须 LD_LIBRARY_PATH=<sysroot>/lib）
- demos 全量扫表，重建 T28 基线（GREEN/CHECK_ONLY/TRAP/SARS_FAIL/loud）
- 按旧簇记录重走，恢复到 TRAP 5 水平：phi 版本化 → UAM spill → phi2a 字面量 seeding → versioned_computed_source → borrow_end → callterm_308 → drop_glue_57_65 → reb_137_193 → uam_09_counting（SetDisc 计数门）
- 验收：驱动编出冒烟（hi.rs 出 mir.json，APPROX=0）；基线表落盘；旧簇全部重提交并推送

## Phase 1：Cargo 工程（提前，原 Phase 5；一切的基础）
- 为什么提前：真实 Rust 代码都活在 Cargo 工程里，不是单文件；没有 Cargo，网络（tokio 依赖）、宏（proc-macro crate）、插件（依赖）都立不住；多文件 mod 树本身也是基础能力。
- 前置依赖：Phase 0 的单文件管线必须先通——Cargo 编排的是能编的 crate，不是替代编译。
- Cargo.toml 解析：package（name/version/edition）、dependencies（path/crates.io/git）、features、bin/lib targets、profile
- 模块树：mod/pub mod、多文件、多 bin/lib、crate 路径解析
- 依赖：path 依赖先行（本地多 crate 工程）；crates.io 下载与版本解析；feature 开关与 cfg(feature) 条件编译
- 构建编排：sars 一次处理整个工程（多 crate → 多 .sa 的链接规则）
- 验收：三 crate path 依赖工程端到端跑通；带 crates.io 小依赖 + feature 开关的工程跑通

## Phase 2：Rust 基础特性（原 Phase 1）
- 类型：整数/浮点/布尔/字符/元组/数组/切片/String/str
- 绑定：let/mut/shadowing/const/static
- 控制流：if/else、loop/while/for、break/continue、match 基础（字面量/通配/绑定/守卫）
- 函数：定义/参数/返回值/递归；impl 基础、方法、关联函数
- 结构体：具名/元组/单元结构体、字段访问、更新语法
- 枚举：基础枚举、带数据枚举；Option/Result 基础（Some/None、Ok/Err、? 基础）
- 所有权基础：移动/Copy/借用（&/&mut）、借用规则、生命周期省略
- 集合与迭代基础：Vec/HashMap（std 映射）、iter/map/filter/collect 常用
- 错误处理基础：panic、unwrap/expect、? 传播
- 验收：基础语料（每特性最小用例）+ demos 基础子集 sa check 全过；有条件时在多文件工程形态下复验

## Phase 3：Rust 进阶特性（原 Phase 2）
- 泛型：泛型函数/结构/枚举、trait bounds、where、关联类型
- Trait：定义/实现/默认方法、dyn trait 对象、derive 基础（Debug/Clone/PartialEq）
- 生命周期进阶：显式标注、多生命周期、'static、生命周期约束
- 智能指针：Box/Rc/Arc/RefCell/Cell/Weak
- 闭包进阶：Fn/FnMut/FnOnce、move 捕获、高阶函数、闭包作参数/返回值
- 迭代器进阶：自定义 Iterator、组合器链、collect 到不同集合
- 模式进阶：解构（结构/元组/枚举/切片）、@ 绑定、or 模式、ref 模式
- 并发基础：thread、mpsc、Mutex/RwLock、Arc 共享、Send/Sync 映射
- 异步基础：async/await、Future 基础（09_async_await 驱动命名问题属本项已知项）
- unsafe 基础：裸指针、unsafe 块/fn、extern "C" 声明（能转的转，不能转的 loud）
- 验收：demos 全量 332 CHECK_ONLY 以上；SARS_FAIL 仅 rustc 拒收类

## Phase 4：网络（原 Phase 3）
- std::net：TcpListener/TcpStream、UdpSocket、地址解析、读写、超时、非阻塞
- HTTP：映射到 sa_plugin_http_client/http_server（复用插件，不重写）；GET/POST、header、body
- 异步网络：tokio 基础（async TcpListener/TcpStream、spawn、select）——Phase 1 的依赖能力已具备
- WebSocket：sa_ws_stream 能力映射，WS echo
- 验收：TCP echo、HTTP GET/POST、UDP 收发、WS echo 四个端到端用例在 sa 上跑通

## Phase 5：宏（原 Phase 4）
- 声明宏 macro_rules!：基础规则、重复匹配 $()*、递归宏
- 标准宏展开验证：println!/vec!/format!/assert! 等（rustc 在 MIR 前展开，多数天然受益，但须逐个验证卫生与递归展开形态）
- 过程宏：自定义 derive、属性宏、函数式宏——驱动侧 proc-macro 宿主支持（编译与加载）
- 验收：macro_rules 常用模式全过；一个自定义 derive 最小闭环跑通

## Phase 6：插件（原 Phase 6）
- sarust 自身插件化交付：sap.json 修正（现指向已删的 tools/mir_json_to_sa.py）、sa plugin install 走通、与 sa 的版本兼容
- Rust 调用 sa 插件：http/db/3d 等插件能力从 Rust API 映射（STD_MAP 扩展到插件 API）
- 插件 FFI 边界：Rust 侧声明 extern 插件函数 → mir2sa 生成对应 SA 调用
- 验收：sarust 可 sa plugin install 安装；Rust 程序调用 http_server 插件、db 插件各跑通一个用例

## 并行开发（8 agent，2026-10-29 用户定）
- 8 agent 并行：每人一个 git worktree + 一个分支；协调员串行合入，每合一个跑定向验证 + 全量扫表，零回退才算过。
- 分工：1）uam_09_counting 重做 2）09 NonNull 类型门 3）118 静态地址类型门 4）313 链门控 5）leak 129 诊断修复 6）leak 182 诊断修复 7）Cargo.toml 解析 + features 8）模块树 + 多 crate 构建编排。
- 资源：容器 8 核 16 线程 50G；单 agent 构建 jobs 上限 2（8×2=16 线程），内存人均约 6G。
- 启动条件：Phase 0 基线先落地（restore 完成 + 驱动重建 + demos 扫表），再从基线分支出去。
