# sa_plugin_rsc 验证记录 (2026-09-30, 容器实跑, 100% Rust / 0 Python)

> 全支持路线：`INVENTORY.md` 为总表；语料库 `corpus/`（24 fns + closures +
> consts + statics）`mir2sa coverage` **100.0%**（零缺口），demo 工程
> `--strict` 全绿
> （`UNSUPPORTED=0`）。落法已对齐 `sa_plugin_sla`（`@extern` 闭包、`&`/`^`
> 前缀、`!` 释放、`alloc`+`store` 数组/Adt、`sa_mem_set` 复写）。

> Python 原型已全部删除（`tools/*.py`、`src/main.rs` 占位），现任实现：
> `rsc/driver/driver.rs`（rustc_private 真劫持）+ `rsc/mir2sa`（纯 Rust
> parse + lower）。落法已对齐 `sa_plugin_sla`（`@extern` 闭包、`&`/`^` 前缀、`!` 释放）。

## 工具链

- `rustc 1.101.0-nightly (c1070d693 2026-09-28)` + `rustc-dev`（`rustup component add`）。
- 构建：`mir2sa` 走 cargo；`rsc_driver` 直调 sysroot rustc（cargo 不解析
  sysroot crate，`build.sh` 用 `-L dependency=$LIB -L $SYSROOT/lib` 自动定位）。
- `LD_LIBRARY_PATH=$SYSROOT/lib:…/lib` 运行（`librustc_driver.so` + `libLLVM`）。

## T0 真 Rust 工程（cargo run）

```
cargo run --manifest-path /tmp/rsc_demo/Cargo.toml
a=hi
sum=10
moved len=4
big
```

## T1 真劫持（rsc_driver, 官方 after_analysis 回调）

```
rsc_driver --edition 2021 /tmp/rsc_demo/src/main.rs --rsc-out /tmp/driver.mir.json
rsc_driver: wrote /tmp/driver.mir.json APPROX=0
```

- `sum` 10 blocks，`main` 33 blocks（与 `rustc -Zunpretty=mir` 文本输出一致）。
- borrowck/typeck 有错则拒绝输出（`err_count>0 → exit 1`），保证后端零错误输入。
- `APPROX=0`：无 `RuntimeChecks` 近似（有则记 `0 /*RuntimeChecks-unsupported*/` 并计数）。
- 命名：`TyKind::FnDef → tcx.def_path_str`，如 `std_io__print`。

## T2 真后端（mir2sa lower, sla 落法）

```
mir2sa lower /tmp/driver.mir.json --out /tmp/driver.sa
wrote /tmp/driver.sa UNSUPPORTED=0
```

- `@extern` 闭包：全部 MIR callee 具名声明（`sa_plugin_sla` emitExternDecl 惯例）。
- `SwitchInt` 单次 move 语义：`Move` discriminant 先绑 `_sw_bbN = ^p` 再分支
  （修过 double-move bug：之前每个 `br ^p` 臂各 move 一次）。
- 多元素 array-init（`[i32; 3]` 定址初始化）：`sci/sa_std/alloc/vec.sa` 惯例
  `alloc` + `store base+off, v as ty`；元素类型/个数/全 Const 三重校验，
  任一不满足回退 UNSUPPORTED（Move 元素永不藏进 store）。
  接受 `-Zunpretty` 文本形（`1_i32`）与驱动 Debug 形（`Val(Scalar(0x…), i32)`）；
  抓到过 `find(", ")` 把 `)` 吞进 hex 的真 bug，由新增 `cargo test` 3 用例锁定。
- `Rvalue::Discriminant` → `discriminant(p)`；`UnwindResume → panic`；
  `FalseEdge/Unwind → Goto(real_target)`；`Cast` 取真目标类型。
- 产物：`examples/real_driver.mir.json` + `examples/real_driver.sa`。

## T3 保真对账（逐点，机械计数）

MIR 侧 23 处 `Move`（逐 place 计数）/ 11 `Ref` / 5 `Drop` ==
SA 侧 23 `^` / 11 `&` / 5 `!` → **DRIVER_FIDELITY_OK**（demo 工程）。
语料库全量：245 / 69 / 29（MIR）→ SA `^`245 / `&`70（含 1 处
`call @sa_mem_set(&_rep_…)` 合成借用，无 MIR 对应，历史锁定文件亦同）/
`!`29 → **CORPUS_FIDELITY_OK**。
每个 SA 所有权标记都 traced 到一条真实 MIR 事实（合成借用除外，已单列）。

## T4 轻文本兜底（mir2sa parse，不链 rustc_private）

`mir2sa parse main.mir.txt --fn main`：33 blocks / 41 stmts / unsupported=0，
与已删 Python 解析器输出 JSON_EQUAL（除 1 处 Python 把 `otherwise` 误收进
targets 的 bug，Rust 版已修正；另 `(_4.0: T)` 投影归一到基 local）。

## T5 反例（大声失败）与单测

- 未知 stmt kind → `bad mir.json: unknown variant …`，`RC=2`。
- `--strict` 下有 UNSUPPORTED → `RC=1`。
- `hi.mir.json → hi.sa`：`UNSUPPORTED=0`。
- `cargo test` 11/11：`scalar_hex_driver_form`、`const_elem_both_forms`、
  `array_init_bb30_shape`（array-init 回归锁）、`repeat_forms`（repeat lowering 锁）、
  `adt_range_two_i32`（Range 2×i32）、`adt_mixed_move_const_bool`（move 混排对齐）、
  `adt_generic_two_moves`（泛型元组双 move 可见）、
  `thread_local_registry_call`（FNV-1a 键稳定 + 注册表调用形状）、
  `asm_mov_copy_exact` / `asm_mov_keeps_move_visible` /
  `asm_non_mov_stays_loud`（asm 精确门控三锁：exact 形、move 可见、他形大声）。

## T7 p_layout v1（本轮：sala/sla 对齐的通用 Adt 落法）

- 对照：`sala/09_rust_compare/03_ownership.html`（`^`/`&`/`!` + Phase1 `&mut`→`&`）、
  `sa_plugin_sla/src/lowering_rules.zig`（`abiTypeSize`/`alignAggregateOffset`/
  `tupleFieldLayout`/`structFieldLayout`/`enum_tag_offset=0`）、
  `sa_plugin_ts/SOLUTION.md`（LayoutTable 思想）、`sci/sa_std/alloc/vec.sa`
  （`alloc` + `store base+off, v as ty`）。未新建任何 std 实现（用户约束：
  缺口只补 `sci/sa_std`；本轮复用既有 `alloc`/`store` 原语，无需补）。
- `Rvalue::Aggregate` 多元素非数组：`lower_adt_init` 按 sla ABI 启发式
  （8 字节对齐，其余紧排；Const 按 `N_TY`/`Val(Scalar, TY)` 取真类型，
  Move/Copy 按 8 字节 `u64` 槽且 `^` 保持可见）`alloc` + 逐字段 `store` +
  `dest = _agg_bbN`。0 元素（unit/niche）→ `dest = 0` exact。
- `SetDiscriminant`：`store base+0, variant as i64`（sla enum tag 位）。
- 实测：`mir2sa coverage examples/corpus.mir.json` 12 → 3 缺口
  （仅剩 ThreadLocal×2 + InlineAsm×1），**99.6%**；`lower` 与 `coverage`
  口径已对齐（修过 0-elem 在 lower 计 UNSUPPORTED 而 coverage 未计的口径差）；
  `hi.mir.json → hi.sa` 零改动（`UNSUPPORTED=0`）。
- 下一步 p_layout v2（需 nightly `rustc-dev`）：`rsc_driver` 内
  `place.ty()` + `tcx.layout_of()` 下发真 `layout/offsets/tys`，泛型单态
  与 niche 布局不再启发式。rosetta 320 文件的 Aggregate-Adt×130 +
  SetDisc×6 届时重跑验证（本轮未重跑，如实）。

## T8 TLS 注册表（本轮：用户约束落地——缺口补在 sci/sa_std，rsc 只做映射）

- 约束：所有未有的 std 必须在 `sci/sa_std` 补充，rsc 禁止原创实现。
  `sci` 侧 `d7c5c812`（cross-platform）：`src/runtime/sa_thread_local.zig`
  （(tid, key) 注册表，零初始化，永不释放；4/4 Zig 单测含跨线程隔离）、
  `sa_std/thread_local.sai`（`@extern sa_thread_local_slot`）+
  `thread_local.sa`（`THREAD_LOCAL_SLOT/U32/U64` 宏，复用既有 `alloc`/
  `load`/`store` + Cell 宽度约定）+ `thread/prelude.sa` 接线 +
  `libsa_std.a` 重建（`nm` 验证符号在档）；`runtime-abi-check` PASS。
- rsc 侧：`Rvalue::ThreadLocal { def }` → 
  `dest = call @sa_thread_local_slot(FNV1a64(DefPath))`（`.sa` 保持无字符串
  字面量；key 即 `TLS_N::{constant#0}…__RUST_STD_INTERNAL_VAL` 的哈希），
  `@extern` 自动补（`sa_mem_set` 同款模式），`cargo test` 新增
  `thread_local_registry_call` 锁定键稳定与调用形状。
- 实测：`mir2sa coverage` 3 → 1 缺口（仅剩 InlineAsm×1），**99.9%**；
  保真 `^`245 / `&`70（含既有合成借用）/ `!`29 不变；`hi.sa` 零改动。
- 下一步唯一缺口只剩 InlineAsm×1（策略 TBD）；p_layout v2（driver 真布局
  下发）与 rosetta 320 文件重跑仍在 backlog（需 nightly `rustc-dev`）。

## T9 asm 精确门控（本轮：最后一个缺口 → 100.0%，真驱动闭环验证）

- 工具链 parity：本机 nightly-1.101 c1070d693 与 T0 记录完全一致；
  `rsc/driver/build.sh` 一次编过。API 取证用编译器当裁判（类型揭示探针）：
  template pieces 实为 `rustc_ast::ast::InlineAsmTemplatePiece`
 （`String`/`Placeholder` 两变体与既有 fixture Debug 自洽），operands 元素
  为 `rustc_middle::mir::InlineAsmOperand`（`Out{place}`/`In{value}` 一次编过）。
- driver v2：`InlineAsm` 终结符增发 `template`（String-piece 拼接，
  `modifier` 置位即记；span 全剥离保 fixture 可移植）、`options` Debug、
  `outs`（首个 Out place 基 local）、`ins`（In 操作数 JSON）。
  控制流行为零改动（沿用既有直落）。
- mir2sa：`asm_mov_copy` 四重门（模板归一 == `mov {0}, {1}`；无修饰符；
  options 为空；单 plain-local out + 单 Copy/Move/Const in）→
  `dest = src // inline-asm mov (exact reg copy)`（x86 mov 不碰 flags，
  Copy/Move 原样透传保 `^` 可见）；他形（含旧 schema 无字段）仍大声
  UNSUPPORTED 并计数。`coverage` 口径同门。
- 实测：`build.sh` 重编驱动 → 重提 `examples/corpus.mir.json`
  （APPROX=0；旧 `sa_plugin_rsc` 路径残留洗净，闭包修饰名重截断，
  stmt/term 总数 428/317 不变，无语义漂移）→ `lower` UNSUPPORTED=0，
  `coverage` **100.0%**；保真 `^`245 / `&`70（含既有合成借用）/ `!`29；
  `hi.sa` 零改动；`cargo test` 11/11。
- backlog 更新：corpus 零缺口；剩余 p_layout v2（真布局）与 rosetta 重跑
  （T6 的 Adt×130/SetDisc×6 在新驱动下可量化关闭，待排期）。

## T6 rosetta 全量（sci 334 demos，rsc 管线实测）

- 官方 `.sa` 直跑：`sa run` 314/334 通过；5 个阴性 demo 按设计拒绝
  （import-cycle ×2、DuplicateDef ×2、CapabilityMismatch ×1）；15 个验证过
  执行挂（bc2sa-LLVM 子集 ×1、沙箱资源 ×7、extern broker 缺失 ×1、
  InvalidAddress 运行时 ×6，属 sci 侧问题，已分类）。
- rsc 驱动：320/334（95.8%）。14 个非驱动责任：外部 crate ×5
  （tokio/futures，需 cargo 工程模式）、nightly 实验特性 ×4
  （specialization/negative_impls/TAIT/try_blocks）、OUT_DIR 环境 ×1、
  demo 自身类型错 ×1（161，官方 rustc 同样拒绝）、无 main.rs ×3。
  （101/104 用 `--edition 2024` 重跑后通过。）
- `mir2sa coverage`：320 文件，3781 stmts + 2794 terms = 6575 项，
  141 缺口 → **97.9%**，成分：Aggregate-Adt ×130、SetDisc ×6、
  ThreadLocal ×4、InlineAsm ×1（另 Repeat/RawPtr/Nop 在此规模零残留）。
  抽查确认多元素 Aggregate 均为 Move 元组/struct（45_config_merge、
  47_tuple_swap），布局活，无误报。

## 剩余缺口（诚实）

1. `cargo build rsc_driver` 不可行（cargo 不解析 sysroot crate），固定走 `build.sh`。
2. `&mut` Phase1 降级为 `&` + Referee（与 `sa_plugin_sla` 已知局限一致）。
3. `alloc <数字>` 直接量与 `store` 元素类型写法待 `sa` 汇编器到货后做汇编级校验
   （当前以 `sci/sa_std/alloc/vec.sa` 现行写法为对齐依据）。
4. rosetta 8 处 Slice-const Aggregate（字符串字面量 `Val(Slice{alloc…})`，
   需 const-eval 提升 alloc 内容；driver const-table 后续工作）与
   p_layout v2（v1 启发式按源码序排布，mixed-size struct/tuple 若被 rustc
   重排则 pad 有差；rosetta 320 文件未发现反例，但理论缺口仍在）。

## T10 rosetta 重跑（新驱动 + 新 mir2sa，T6 缺口关闭量化）

- 方法：`sci/demos/rosetta` 331 个 `main.rs` → 新 `rsc_driver`
  （nightly-1.101 c1070d693）直提（先 2021 edition，失败转 2024，与 T6
  同口径）→ 新 mir2sa `coverage` 逐文件聚合。驱动 320/331 通过；11 个
  驱动失败与 T6 已知分类一致（async 系、specialization/negative_impls/
  TAIT/try_blocks、OUT_DIR 环境）。
- 结果：同 320 文件、同 3781 stmts + 2794 terms = 6575 项，缺口
  **141 → 9**（**97.9% → 99.9%**）：
  - Aggregate-Adt ×130 → ×8（v1 启发式 + ZST 跳过关闭 122；剩余 8 个全含
    `Val(Slice {alloc…})` 字符串字面量常量，保持大声）；
  - SetDisc ×6 → 0；ThreadLocal ×4 → 0；InlineAsm ×1 → ×1（`117` 的
    `/* native escape */` 非 mov 形，保持大声，正确）。
- corpus 侧：`lower`/`coverage` 重跑仍 UNSUPPORTED=0 / 100.0%（锁定产物
  字节一致）；`cargo test` 12/12（新增 `adt_zst_skipped`）。
