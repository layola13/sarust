# Rust std → sci/sa_std 投影复用表 (normative for sa_plugin_rsc)

> 原则 (用户指示): **所有 std 投影到 `sci/sa_std`, 复用, 理论上尽可能复用。**
> rsc 不新建任何 std 实现, 只做 `Rust 路径 → sa_std 路径` 的映射 +
> 薄适配层 (ABI 布局/名修饰/extern)。缺口走 `sa sla build` 的现有兼容回退,
> 不在 rsc 里另起炉灶。

上游: `/content/rust/library/{core,alloc,std}`。
下游复用目标: `/content/sa_all/sci/sa_std/**` (已存在 `core/`, `alloc/`, `collections/`,
`io/`, `os/`, `ffi/`, `marker/`, `mem/`, `num/` 等, 与 Rust std 同构)。

## 顶层三仓映射

| Rust 上游 | rsc 输入侧 | 复用目标 (sci/sa_std) | 备注 |
|---|---|---|---|
| `library/core` | `core::*` | `sa_std/core/*` + 顶层 `*.sa` (`option.sa`, `result.sa`, `marker.sa`, `mem.sa`, `ops.sa`…) | `Option/Result/Box/Rc/Arc/Cell/RefCell/Iterator` 已在 `sa_std/core/*.sa` 落地; 直接 `@import` |
| `library/alloc` | `alloc::{vec,string,collections,…}` | `sa_std/alloc/*` + `sa_std/collections/*` + `sa_std/cow.sa`, `string`/`vec` | `Vec/String/VecDeque/LinkedList/BinaryHeap/BTreeMap/Set/HashMap/HashSet` 均有 `.sa/.sal` 对 |
| `library/std` (`io/fs/net/env/thread/sync/time…`) | `std::*` | `sa_std/io*`, `fs.*`, `net.*`, `env.*`, `join_handle.*`, `instant/duration.*`, `ffi.*`, `os/*` 等 | `print.sai/io.print` 为 hello_world 已验证路径; 线程/网络走 `sa` host ABI + Airlock, 与 `sa_plugin_sla` 同策略 |

## 明细投影 (高频先行, 全量渐进)

| Rust 符号 | MIR 侧可见形状 | 投影到 sa_std | SA 侧调用形状 |
|---|---|---|---|
| `String::from(&str)` | `Call(func=alloc::string::String::from, args=[Move(_1)])` | `sa_std/alloc/string.sa` + `sa_std/alloc/prelude.sa` | `call @string_from(&LIT)` / `call @sa_alloc_string(...)` (以实际 `string.sa` 符号为准, rsc 只记映射, 不硬编码实现) |
| `Vec::new/push/len` | 同上 (`alloc::vec::Vec::push`) | `sa_std/alloc/vec.sa` + `sa_std/collections/*` | `push(^v, x)` UFCS, `len(&v)` 自动借用 (复用 sla 的 auto-borrow 规则, 见 `sa_plugin_sla/docs`) |
| `Option<T>/Result<T,E>` + `?` | `Rvalue::Aggregate` + `Terminator::Assert` | `sa_std/core/option.sa`, `result.sa`, `error.sa` | 后缀 `?` 复用 sla lowering (分支+自动 `!` 清理), rsc 只透传 MIR 的 `Assert/Return` |
| `Box<T>/Rc<T>/Arc<T>/Cell/RefCell` | `Rvalue::Aggregate` / `Ref` | `sa_std/core/box.sa`, `rc.sa`, `arc.sa`, `cell.sa`, `refcell.sa`, `weak.sa` | 智能指针识别复用 `sa_plugin_sla/src/lowering_rules.zig::smartPointerType` 系列, 不在 rsc 重写 |
| `&T/&mut T` | `Rvalue::Ref(Shared/Mut)` | 无需 std, 直接 SA `&` 前缀 | Phase1 `&mut` 降级为 `&` + Referee 兜底 (与 `sa_plugin_sla` README 已知局限一致); Phase2 才引入真 `&mut` |
| `Drop::drop` / 析构 | `Terminator::Drop(place)` | `sa_std/core/cleanup.sa` + 各类型 `drop glue` | `!place`, 分支汇合的 Phi 由 sla 侧自动平衡 (rsc 透传 MIR 的 `Drop/Goto` 即可) |
| `print!/println!/panic!` | `Call(func=std::io::stdio::print…)` | `sa_std/io/print.sai` (`@sa_print_bytes`) | `call @sa_print_bytes(&MSG, len)` (hello_world 已验证) |
| `fs/read/write/metadata` | `Call(func=std::fs::…)` | `sa_std/fs.sa` + `os/fd.sa` | `@extern` + `@ffi_wrapper` Airlock (复用 `sa_plugin_ts/SOLUTION.md` 的 FFI 策略) |
| `net/tcp/udp/http` | `Call(func=std::net::…)` | `sa_std/net*.sa` + http 插件 | 同上, 走 host ABI |
| `mem::size_of/transmute` | `Rvalue::Cast / Intrinsic` | `sa_std/mem.sa`, `core/mem.sa` | `abiTypeSize/structFieldLayout` 复用 `lowering_rules.zig` 的 ABI 布局规则 |
| `marker::{Send,Sync,Copy}` | trait bound (MIR 类型元数据) | `sa_std/marker*.sa` | 仅作检查, 不发射代码 (rustc 已验, rsc 透传) |
| `thread_local!` / `LocalKey<T>::with` | `Rvalue::ThreadLocal{def}`（内部 `EagerStorage` 访问体）+ `Call(LocalKey::with)` | `sa_std/thread_local.sai` + `thread_local.sa`（`sci@d7c5c812` 真 per-thread 注册表；Cell 宽度复用 `core/cell.sa` 约定） | `dest = call @sa_thread_local_slot(FNV1a64(DefPath))`（rsc 只记 DefPath→key 映射，不硬编码实现；`@extern` 自动补） |

## 不复用的东西 (有意)

- `rustc_codegen_llvm/cranelift/gcc/ssa` 整条后端: 被 `gen_sa` 替换, 仅保留 `rustc` 前端 + MIR。
- LLVM bitcode 猜测 (`sa_plugin_bc2sa`): 退役为实验对照, 新代码不依赖。
- 任何在 `sci/sa_std` 已存在的集合/智能指针/IO 实现: 禁止在 rsc 内复制第二份。

## 落地顺序

1. `String/Vec/Option/Result/&/Drop/print` (本骨架 `hi.*` 已覆盖形状)。
2. `fs/net/env/thread` (Airlock ABI, 与 ts 插件同策略)。
3. 全量 `core/alloc` 单态化 (`monomorphize` 复用 sla 侧, MIR 的泛型实例已由 rustc 展开, rsc 只需透传 `DefId + Subst` → 修饰名)。
