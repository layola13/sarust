# sa_plugin_rsc — Rust → SA (rsc → rs → sa), 100% Rust, 0 Python

> 状态：真跑通。官方 rustc 做检查并产 MIR，`rsc_driver`（rustc_private）
> 劫持 MIR → mir.json，`mir2sa`（纯 Rust）→ `.sa`，与 `tsgo → sci` 同构。
> `bc2sa`（猜 LLVM bitcode）退役为实验对照。

```
.rs 源码 (cargo 工程, 稳定写法, 任意版本)
  |
  +--(官方 rustc 前端: parse / resolve / hir / typeck / borrowck, 100% 官方)--> MIR
  |
  +--(rsc_driver: rustc_driver + rustc_private, after_analysis 劫持)--> mir.json
  |
  +--(mir2sa lower: mir.json --> .sa, 复用 sci/sa_std, 落法对齐 sa_plugin_sla)--> .sa
```

MIR 就是 Rust 的 Typed IR：`move / & / &mut / drop / StorageLive/Dead`
全标好，borrow checker 已验完。后端只做机械翻译（`sa_plugin_sla` 的
`&`/`^`/`!` 前缀与 `@extern` 闭包惯例，见 `STD_MAP.md`）：

```text
Operand::Move(p)  =>  ^p      Operand::Copy(p) -> p (Referee 复验)
Rvalue::Ref(_,_,p)=>  &p      Terminator::Drop(p) -> !p
```

## nightly 押注策略（只押 MIR，不押整个 rustc）

1. 只碰 `after_analysis` 之后的 `optimized_mir`，不碰 HIR/Typeck（最稳的层）。
2. `rust-toolchain.toml` 浮动 `nightly`（容器内已装 rustc-dev）；CI 再 pin
   exact 日期。用户 `.rs` 可用任意版本写，驱动内部用 pin 的 nightly 解析。
3. 文本兜底：`rustc -Zunpretty=mir` + `mir2sa parse`（纯 Rust，不链
   rustc_private，API 再变也能解析），与重链接驱动输出同一 schema。

## 目录

```
sa_plugin_rsc/
├── README.md / STD_MAP.md / TEST_LOG.md
├── rust-toolchain.toml / sap.json
├── rsc/                        # Rust workspace
│   ├── Cargo.toml              # members = ["mir2sa"]
│   ├── mir2sa/                 # 纯 Rust 后端: parse (MIR文本→json) + lower (json→sa)
│   └── driver/                 # rustc_private 真驱动
│       ├── driver.rs           # Callbacks::after_analysis 劫持 optimized_mir
│       └── build.sh            # 直调 sysroot rustc 构建 (cargo 不解析 sysroot crate)
└── examples/
    ├── hi.rs / hi.mir.json / hi.sa            # 最小单测夹具 (lower UNSUPPORTED=0)
    └── real_driver.mir.json / real_driver.sa  # 真劫持产物 (cargo demo → driver → mir2sa)
```

## 真跑（本机实测，见 TEST_LOG.md）

```bash
# 0. 真 Rust 工程
cargo run --manifest-path /tmp/rsc_demo/Cargo.toml   # a=hi sum=10 moved len=4 big
# 1. 真劫持（官方 API，borrowck 失败则拒绝输出，保证零错误输入）
./rsc/driver/target/rsc_driver --edition 2021 /tmp/rsc_demo/src/main.rs \
    --rsc-out /tmp/driver.mir.json --crate-type bin  # APPROX=0
# 2. 真后端（sla 落法：@extern 闭包 + &/^/!）
./rsc/target/debug/mir2sa lower /tmp/driver.mir.json --out /tmp/driver.sa
# 保真：MIR 侧 23 处 move / 11 borrow / 5 drop == SA 侧 ^/&/! (逐点对账)
```

## 上游浅克隆

```bash
git clone --depth 1 https://github.com/rust-lang/rust /content/rust
```

rsc 只 pin 它的 MIR 层（`rustc_middle::mir` 5 年稳定），API 取证直接读该
checkout 与已装 nightly 自带的 `rustc-src`。
