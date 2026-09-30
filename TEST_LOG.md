# sa_plugin_rsc 验证记录 (2026-09-30, 容器实跑, 100% Rust / 0 Python)

> 全支持路线：`INVENTORY.md` 为总表；语料库 `corpus/`（24 fns + closures +
> consts + statics）`mir2sa coverage` **84.6%**（115 具名缺口；T14 起以
> `sa check` 为准绳，见 T14）。验收集：sci 321 文件（6587 项，825 缺口）+
> sla 298 文件（9432 项，1959 缺口）。三集 parse-trap 归零（逐函数普查），
> 剩余 trap 全部 Referee 层（下一阶段）。
> 落法已对齐 `sa_plugin_sla`（`@extern` 闭包、`&` 前缀、`!` 释放、
> `alloc`+`store` 数组/Adt、`sa_mem_set` 复写；`^` 仅合法于 call 实参，
> 赋值位裸写）。

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
- `cargo test` 44：`scalar_hex_driver_form`、`const_elem_both_forms`、
  `array_init_bb30_shape`（array-init 回归锁）、`repeat_forms`（repeat lowering 锁）、
  `adt_range_two_i32`（Range 2×i32）、`adt_mixed_move_const_bool`（move 混排对齐）、
  `adt_generic_two_moves`（泛型元组双 move 可见）、
  `thread_local_registry_call`（FNV-1a 键稳定 + 注册表调用形状）、
  `asm_mov_copy_exact` / `asm_mov_plain_copy` / `asm_non_mov_stays_loud`、
  `asm_inout_passthrough_117` / `asm_inout_split_places` /
  `asm_inout_non_passthrough_stays_loud`（asm 六锁）、`adt_zst_skipped`、
  `adt_v2_reordered_tuple` / `adt_v2_enum_payload_absolute` /
  `adt_v2_arity_mismatch_falls_back`（v2 三锁）、`adt_str_lit_fat_ptr` /
  `adt_str_lit_gates`（str 内联两锁）、`sa_ident_sanitizes` /
  `switchint_chain_shape` / `assert_shape` / `unreachable_becomes_panic` /
  `typed_header_and_extern` / `cast_copy_vs_convert`（形状六锁）、
  `rpo_loop_back_edge` / `rpo_diamond` / `rpo_unreachable_appended` /
  `dom_seeds_diamond`（order 四锁，`order.rs` 内）、`classify_shapes` /
  `frees_simple_leak` / `borrow_ordering` / `moved_not_freed` /
  `branch_local_never_freed_at_join`（drop 五锁，`drop.rs` 内）、
  `call_spill_ty_maps` / `synth_base_spills_as_ptr` / `no_copies_no_spill`
  （spill 三锁，`spill.rs` 内）、`single_def_passthrough` /
  `chain_versions` / `join_conflict_sentinel` / `stmt_plus_asm_out_reaches` /
  `loop_carried_conflicts` / `asm_versioned_names_pass_gates`
  （version 六锁，`version.rs` 内）。

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
3. 发射形状汇编级校验已落地（T14）；剩余为 Referee 层程序（仿射/借用/泄漏，
   T14 已计数）与值层缺口（INVENTORY 表）。
4. 两仓官方拒收 demo（T13 已逐项定性：外部 crate / 未完成 nightly 特性 /
   方言示意 / 缺构建产物）：输入在 rustc 即无 MIR，属管线射程之外；
   其中可 cargo 化的（tokio 系）待 `rsc build` 工程模式立项。

## T12 Slice-const 内联（本轮：rosetta 9 → 1，全部可判定缺口关闭）

- driver：`str_const_bytes`（`Const::Val(ConstValue::Slice{alloc_id,meta})`
  + 类型 `&str` + `global_alloc→Memory→get_bytes_unchecked` + UTF-8 校验，
  取证自 `mir/consts.rs:35` 与 `interpret/allocation.rs:575`），操作数 JSON
  增发 `str_bytes/str_len`（仅可解析时出现；旧 fixture 字节兼容）。
  `operand_json` 穿 `tcx`（8 处调用点，生命周期 `'a` 统一）。
- mir2sa：`Operand::Const` 增可选两字段（call 参数等渲染路径原样透传，
  仅 ADT 装配消费）；`FieldPlan::StrLit`（16B/8B 对齐，slice.sal 布局）：
  `_str_{bid}_{i} = alloc len` + 逐字节 `store … as u8` + 字段双写
  `(ptr as ptr, len as u64)`；>64B/计数失配/非 str 文本 → None 大声。
- 实测：corpus 重提重落仍 100.0%（无 str 常量，锁定产物仅 v2 标记差分）；
  rosetta 320 文件缺口 **9 → 1**（**99.9% → 100.0%**，仅剩 `117` 非 mov
  asm，保持大声，正确）；`cargo test` 17/17（新增 `adt_str_lit_fat_ptr` /
  `adt_str_lit_gates`）。
- 诚实记录：曾试图用 `sci` 的 `sa check` 驗新发射形状，手写 SA 探针连
  `n = add 0, 32` 都过不了（SA 手写语法另学，见 sala 03 章）；且既有
  `corpus.sa`（`{constant#0}` 函数名）与 `alloc 32` 字面量同样不过 check——
  恰为既有缺口 #3（汇编级校验待到货）的覆盖范围。新发射与既有
  array-init/sa_std 宏体逐行同形，无新增 unverified 形状。

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

## T11 p_layout v2（本轮：真布局下发，关 v1 理论 pad 差）

- 动机：v1 按源码序排布，rustc 会重排字段（T10 已披露为理论缺口）。
  实锤：`main bb13` 三元组真布局 `[24,0,28]/32`（v1 给出 `17`）；
  `main bb0` 枚举 payload 真布局 `[4,8]/12`（v1 给出 `8`，丢 tag-gap）。
- driver：`aggregate_layout`（`dest.ty(local_decls)` + `layout_of`
  fully-monomorphized + 枚举 `for_variant`；Primitive/Union/arity 失配/
  泛型 → None，提取永不失败）。API 取证：`layout_of` 在本 nightly 收
  `PseudoCanonicalInput{typing_env,..}`（`offload_meta.rs` 同款），
  `for_variant` 需 `&LayoutCx`（`ty/layout.rs:317`），`FieldsShape::offset`
  在 `rustc_abi/src/lib.rs:1738`（`Union→0` 故显式排除，`Primitive` 会
  panic 故 `count` 守卫 + 形状白名单）。
- mir2sa：`Rvalue::Aggregate` 增可选 `layout{size,offsets}`（serde 默认缺席，
  旧 fixture 照解析）；`lower_adt_init` 布局存在且 arity 相符即原文采用
  （`total` 取 max 防缩水），否则 v1；`coverage` 同口径；单测
  `adt_v2_reordered_tuple` / `adt_v2_enum_payload_absolute` /
  `adt_v2_arity_mismatch_falls_back`。
- 实测：corpus 重提重落仍 100.0%，差分仅两处修正（见上）+ 标记翻 v2
  （6 v2 + 1 v1=`f_generic` 无布局诚实回退）；保真 `^`245 / `&`70 /
  `!`29 不变；rosetta 320 文件重跑缺口 9 → 9（零回归；11 驱动失败集不变）；
  `cargo test` 15/15。

## T13 验收：两仓 Rust demo 全支持（inout 门控 + 全量测量 + 失败定性）

- 验收口径：`sci/demos/rosetta` 与 `sa_plugin_sla/demos/rosetta` 的全部
  `main.rs`（合计 346 个独立目录）经新驱动直提 + 新 mir2sa `coverage`，
  rustc 自身可独立编译的 demo 必须零缺口。
- inout 门控（关最后一个 sci 缺口）：sla-117 的 `.sla` 镜像与 README 定调
  为“value-stable escape”（测试断言 `got == 7`，`main.sa` 直接消解）。
  driver：`InOut{in_value,out_place}` 双边下发 + `inout` 标记（余下
  `Const/SymFn/SymStatic/Label` 具名标记，`mir/syntax.rs:1056` 全变体覆盖）；
  mir2sa：注释-only 模板（`/*…*/` 剥离，非闭合大声）+ 空 options +
  单 out/in → 同 local 零指令（注释），分 local 补 `out = in`（`^` 可见）；
  mov 门收紧（拒 inout）。`cargo test` 新增三锁，20/20。
- 全量：sci 集 321 文件 6587 项零缺口；sla 集 298 文件 9432 项零缺口；
  corpus 745 项零缺口。合计实测 619 文件、16764 项、`UNSUPPORTED=0`。
- harness 修正：`OUT_DIR=<demo dir>` 透传（修 `195`，demo 自带
  `generated.rs`；sla/195 未带产物，仍不可独立编译，见下）。
- 失败定性（逐项经官方 rustc 复验，均拒收，与驱动无关）：
  - 外部 crate（需 cargo 工程）：sci×5 + sla×5（tokio/futures 系）；
  - 未完成 nightly 特性：sci×4 + sla×4（specialization / negative_impls /
    TAIT / try_blocks）；
  - 自定义属性宏（需 proc-macro crate）：sla/193；
  - demo 自身类型错：sci/161（rustc 同拒）；
  - sla 方言示意文件（非合法 Rust）：310/311/312；
  - 缺构建产物：sla/195（无 shipped generated.rs）；
  - 无 main.rs：sla/314/315（纯 `.sla`，无 Rust 输入）。
- backlog 更新：corpus/rosetta 两仓零缺口后，剩余 p_layout v2 已落地；
  通用未竟：`sa` 汇编级校验（缺口 #3，已立项为 T14）、`&mut` Phase2（sla 触发）。

## T14 asm_verify（本轮：以 `sa check` 为准绳，发射形状全过汇编器）

- 动机：T13 的零缺口是 MIR-kind 口径；发射行从未经汇编器，属未验证宣称。
  方法：sala 03/04/08 章 + 探针二分（`sa check` 微文件）定合法形状 →
  逐函数 `sa check` 普查 → 修发射端 → 复测。判据分层：parse 类 trap
  （ForbiddenSyntax/UnknownRegister/CapabilityMismatch/IllegalUnsafeContext/
  UnsupportedType）必须归零；Referee 类（UseAfterMove/MemoryLeak/
  BorrowConflict/PhiStateConflict/RegisterRedefinition）属仿射/生命周期
  程序，计数移交 backlog。
- 分模块（AGENTS.md 规则落地）：`mir2sa/src/main.rs` 2126 行 →
  mir/parse/render/render_util/layout/asm/cast→asm/const_util/order/lower +
  main（最大逻辑文件 417 行）；`driver.rs` 815 行 → util/place/tyinfo/
  emit + driver（最大 468 行）。拆分前后构建 + corpus 产物字节一致。
- 形状修正（探针验证，每项皆有 `sa check` OK 证据）：
  - 标签 `bbN:` 非法 → `L_bbN`（RPO 排放序保文本先定义后使用；asm 补显式 `jmp`）；
  - `br x == v` 非法 → `eq`+双目标 br 链（`eq` 非消费，复用安全）；
  - `assert c` 非法 → `eq`+`br`+数字 `panic`（复用 sa_core ASSERT_EQ 形；`expected` 由 driver 下发）；
  - `@{closure#0}` 非法 → `sa_ident` 清洗（定义与调用点同函数）；
  - `_x = ^_y` 赋值位非法（`=` 本身即 move）→ 裸写，`^` 仅留 call 实参（后证实与 plain-param 声明 mismatch，亦改裸写；`^` 仅存于 store 值位）；
  - 行尾 `//` 非法 → 注释全部整行化（含外部文本 `flat_comment` 展平）；
  - `discriminant()`/`*p`/call 形 BinOp 非法 → `load +0`/`plain copy`/小写助记符；
  - `eq ^x` 非法 → Move 先绑临时；`_x = Val(..)` → 十进制化（顺手修 `false` 字面量泄漏）；
  - `unreachable` 终结函数文本 → 改 `panic(16xx)`（含 diverging call）；
  - 调用须类型化 `@extern`（无参声明调有参必 CapabilityMismatch）→ driver 下发函数签名 + callee sig（`Val(ZeroSized, FnDef)` 形取证），`void` 调用裸写；
  - `_proj` 占位（双括号投影漏网）→ `p.local` 精确取基（全集零残留）；
  - `alloc 32`/`store +0`/`_N` 命名经探针合法，无需改动。
- 诚实计数（lower/coverage 双镜像，同函数判定）：overflow-元组/符号比较/
  Unsize 转换/不可判定常量（Unevaluated/byte-ref/call 实参）/无 sig 调用/
  重绑定（支配集种子，`order::dom_seeds`）/128 位签名，一律大声 + 占位续行。
- 实测：corpus 115 缺口（84.6%，分类见 INVENTORY）；sci 825/6587；sla
  1959/9432。三集 parse-trap 归零（corpus 逐函数 40/40，rosetta 619 整文件）；
  Referee 层：corpus（UAM 11/Leak 10/Borrow 5/Phi 2）+ sci（Leak 185/UAM 103/
  Phi 20/Borrow 9）+ sla（Phi 114/UAM 108/Leak 60/Borrow 12）+ 9 个全绿文件；
  `cargo test` 30/30。
- backlog（具名程序）：const-eval（Unevaluated/byte-ref 38+10）、溢出元组解构（27）、
  SSA 版本改写（Rebind 23）、调用实参提升（10）、driver 符号性（9）、fat-meta（3）、
  Unsize（5）、Referee Drop 胶水（仿射全集，T15 立项）。

## T15 referee_drop-1（本轮：仿射取证 + 出口释放，泄漏 255→2）

- 仿射消费表（`sa check` 探针逐项取证）：`x = y` 移动源（`!` 已移动源报
  UseAfterMove）；call/store/eq/load/br 均为共享读（同寄存器复用 OK）；
  `&y` 锁定源（生借用未释禁 `!y` → BorrowConflict）；`!r` 释放；
  `^` 仅 call 实参/store 值位合法（赋值位/eq 内非法）；
  `alloc N`/`store +off`/`_N` 命名合法；`return` 可后随标号；
  `unreachable` 终结函数文本（后继禁排），`panic` 不终结。
- `drop.rs`（新模块，`exit_frees` + 5 单测）：Return 块出口释放；
  候选 = 单定义（无条件前缀）+ 支配出口 + 全程未移动/未释放（含参）；
  借用拓扑排序（借者先释）；任一存疑即跳过（宁漏不新 trap）。
  关键 bug 史：首版缺支配检查（分支局部定义在 join 出口被释 →
  UnknownRegister，`_constant_0_` 事件），补支配+无条件前缀后解决；
  另修 Repeat 漏绑 dest（`_1 = _rep`，54/199 事件）。
- 实测：corpus 全绿 3→20（Leak 10→0）；sci 全绿 5→189（Leak 185→1）；
  sla 全绿 4→63（Leak 60→1）。parse-trap 保持归零；剩余 Referee
  按上表移交（UAM/借用/Phi 需重借与路径敏感程序）。
- 残 2 泄漏（181/188 `_sw` 临时量）：定义于条件区、join 出口可达但非支配
  ——需 use-analysis（末次使用后即释），下期。
- `cargo test` 35/35（含 drop 5 锁 + order 支配锁）。

## T16 uamn-1/2（本轮：const-prop + spill/reload，UAM 220→127）

- 分类普查：UAM 源定义 `const-def`（含占位 `0`）103 → const-prop 重物化
  字面量（RPO 前向 map + 占位回填；`Use` 位 Move/Copy 统一处理）。
  剩余 `call-def`（调用结果复用，副作用禁重算）与 `other`（计算值复用）
  走 spill。
- `spill.rs`（新模块）：合成缓冲（`_agg_*`/`_rep_*` 恒 ptr）+ 有 sig 调用
  结果（sig ret 映射槽类型），Copy 位重载（`load slot+0`），Move 位直传；
  单定义门（多定义需版本化，否则槽重定义）+ 响亮路径补槽（占位一致）。
- 实测：corpus 全绿 20→23（UAM 14→9）；sci 全绿 189→226（UAM 102→57）；
  sla 全绿 63→177（UAM 104→61，UnknownRegister 95→0——多定义槽收敛 +
  响亮补槽）。parse-trap 保持归零；无回归（既有全绿文件逐个复验仍绿）。
- `cargo test` 38/38（含 spill 3 锁）。
- 残留 UAM 全系多定义/版本化类（param 重写、分支 join、循环携带）与
  重借类（borrow-copy），下期（sla merge-slot 范式：分支写槽/join 重载，
  需 driver 局部宽度表）。

## T17 versioning（本轮：SSA 版本化改写， rebinding 归零）

- `version.rs`（新模块）：MIR→MIR 预变换。收集 def 点（Assign/Call/asm-outs）→
  RPO 定编号（`_N_vK`）→ reaching-definitions 数据流（OUT=块内最新，IN=前驱
  合并，fixpoint）→ 改写 uses（同块程序序优先，否则 IN 唯一版；零/多版
  分走 keep-name/Conflict 哨兵）；place 位冲突提为整 rvalue/stmt/终结符
  Unsupported（place 无哨兵形）；Ref 空臂、asm-out 查找、`is_plain_local`
  版本名（`_1_v1` 过门）三 bug 修。
- lower/coverage 同构消费版本化 MIR（parity by construction）；单定义函数
  零改动快通（锁文件稳定）。
- 实测：corpus loud 115→104（Rebind 23→2）；sci 全绿 226→228；
  sla 全绿 177→180；117/21 等版本冲突文件转全绿或诚实大声；
  parse-trap 保持归零；`cargo test` 44/44（含 version 5 锁）。
- 残留：合流/循环携带的 Conflict 大声（需 phi，sla merge-slot 范式为远期
  答案）；UAM 余量为 borrow-copy 与计算值复用类。

## T18 borrow-end（WIP：可达 Drop 点借用终结落地，cleanup 去重待续）

- 新模块 `borrow_end.rs`（`plan_drops`，6 单测）：MIR `Drop(p)` 处，若全部
  借用者 `b = &p` 满足单定义 + 定义块支配 Drop 块 + Drop 后无使用 +
  无重借用链 + 无 MIR-Drop(b) + 无 Conflict 标记 + Drop 块 entry 可达，
  则先释借用者（`!b` 再 `!p`）；任一不满足则整站保持原形并记
  `DropBorrowLive` 大声（lower/coverage 同构消费版本化 MIR，parity 成立）。
  drop.rs 出口逻辑把已插入 `!b` 视为已释放，自动跳过（无双释）。
- 实测（corpus 40fn，逐函数 `sa check` 普查）：BorrowConflict **5→0**；
  `cargo test` **50/50**（+6 borrow_end 锁）；coverage loud 104→116
  （+12 DropBorrowLive，lower/coverage 双口径一致）。
- 诚实记录（回归表象，非回归实质）：UAM 9→14。新增 5 例全是 cleanup
  路径重复 `Drop`（如 f_generic bb5 `!_1`）：此前被 BorrowConflict 掩盖
  （Referee 按文本序首 trap 即停）。探针取证 Referee 模型：return 后仍
  fallthrough 扫描（`ft` 探针：return 后 `!_1` 报 UAM；`fu` 新定义则过），
  panic 路径不查泄漏（`pl` 探针 OK），菱形双臂各释一次合法（`dd` 探针）。
  推论：cleanup 块运行时不可达（panic 中止，无 unwinding），其重复 `!p`
  注定与主路径释放冲突——下期做 cleanup 去重（不可达 Drop 站省略 `!p`），
  届时 UAM 回落、Borrow 保持 0。停工前未做，保持树为诚实 WIP。
- `examples/corpus.{sa,coverage.txt}` 已重落（锁定产物含新 `!b` 行与
  drop-borrow-live 注释）；`hi.sa` 零改动。

## T18b cleanup 去重（phase 2：不可达 Drop 站省略释放）

- 判据（全部由 `sa check` 微探针取证，探针件见下）：(1) `bt1`/`dd` 菱形
  两臂各释一次合法（真赋值即可，非路径敏感）；(2) `pl` panic 路径不查泄漏；
  (3) `fu` return 后新定义合法 → 判定 Referee 为「return 之后仍 fallthrough
  线性扫描」而非路径敏感；(4) `ft` 同处再释同一 reg 报 UAM。据此：cleanup
  块运行时不可达（`panic` 中止，一期无 unwinding），而 Referee 仍会看到其
  重复 `!p` 报 UAM——唯一出路是**不发这条 `!p`**。
- 实现：`borrow_end.rs` 增 `DropPlan.cleanup`（entry 不可达的 Drop 站），
  在借用门控**之前**判定（不可达站也无需释借用）：lower 省略释放、发注释
  `// cleanup drop _p (omitted: main path owns the release)`；coverage 对称
  不记缺口（去重是修复不是缺口）。新增单测
  `cleanup_without_borrow_also_deduped`（+1 → **51/51**）。
- 实测（corpus 40fn）：BorrowConflict **0**、UseAfterMove **9**（回到 T17
  基线，T18 的 +5 假回归消解）、全绿函数 **23→28**（5 个 borrow 函数由红转
  绿）、PhiStateConflict 3 不变、parse-trap 归零；`cargo test` **51/51**；
  lower UNSUPPORTED **116→104**（12 条记账缺口清零，与 T17 基线持平）；
  corpus.sa 删 29 行（16 处 cleanup 去重 + 13 处 `!b` 前置位置修正）。
- 探针件（`scratch/probes/*.sa`，scratch/ 已 gitignore，结论已抄入本节与
  `borrow_end.rs` 模块文档）：bt1/dd/pl/fu/ft/g1/g2/g3/pb1..pb4/t1..t3。
- 残留不变：UAM 9（borrow-copy 与计算值复用类，见 INVENTORY）、
  PhiStateConflict 3（合流/循环携带 Conflict，需 phi 范式）。

## T19 spill 扩展：cast/借用 dest 参与 reload（UseAfterMove 9→6，全绿 28→31）

- 缺口分类（9 例 UAM 逐一定位）：5 例是「Box 解引用检查链」——rustc 生成
  `_6 = cast(copy _2)` / `_7 = cast(copy _6)` / `_12 = cast(copy _6)` 形态，
  同一指针被 plain-assign 复制 2-3 次；spill.rs 原本只覆盖 `Rvalue::Use` 与
  单元素 Aggregate 的 Copy 源，且只给 *call dest* 与 `_agg_/_rep_` 合成缓冲
  发槽位——cast dest 与借用 dest 从不 spill，于是第二次 assign 必 UAM。
- 实现（spill.rs + render.rs，+5 单测 → **56/56**）：
  (1) `cast_spill_ty`：指针拼写→`ptr`，标量过 `sa_scalar_ty`（bool→u8、
  usize→u64），128 位与聚合目标返回 None（无 SA 槽类型）；
  (2) cast dest 也是 copy-use 源，且在 `lower_cast` 判定可降级时按目标类型
  发槽位（Cast 臂内联 `alloc 8` + `store`，与 `_rep_` 合成槽同形）；
  (3) 借用 dest（`b = &p`，非 ZST）同样 spill 为 `ptr`——新增探针发现
  **被移动过的 reg 上 `!r` 报 UnknownRegister 而非 UAM**（`m1` 探针：
  `_77 = _78` 后 `!_78` → UnknownRegister），根因是 `_77 = Aggregate([Move
  (_78)])` 消费了借用寄存器，而 borrow-end 要在 `!_p` 前发 `!_78`；故
  `consuming_use_targets` 把 Move 位置也纳入源集合，Use/Aggregate/Cast 三臂
  统一走 `spill_reg()` 重载。
- 实测（corpus 40fn 逐函数 `sa check`）：全绿 **28→31**、UAM **9→6**、
  PhiStateConflict **3→2**（f_parse 的合流冲突被 spill 重载改写后消解）、
  parse-trap 归零；loud 104 不变（86.0%）；`cargo test` **56/56**。
- 探针新证（已抄入本节与 spill.rs/render.rs 注释）：r4/r5 —— `return <reg>`
  合法且**消费**该 reg（r4 ok），`return 0` 则泄漏它（r5 MemoryLeak）。
  这是下一特性 T19b 的依据：当前 Return 恒发 `return 0`，既丢返回值又漏值。
- 诚实记录：f_parse 露出 1 例新 MemoryLeak `_0_v0`（合流版本 `_0_v0/_0_v1`
  各定义在一支，谁都不支配 join，drop.rs 按支配门不敢释放）——旧版被
  PhiStateConflict 掩盖（Referee 首 trap 即停），属「需 phi」同一族限制的
  泄漏面，**不新增能力缺口**，但已具名入册（INVENTORY trap 表）。

## T19b 返回值保真：`return <reg>` 取代恒 `return 0`

- 问题：Return 臂恒发 `return 0`——**每个非 void 函数都返回 0**（返回值全丢，
  且被返回的局部量按 `return 0` 语义泄漏）。探针 r4/r5 证：`return <reg>`
  合法且**消费**该 reg（r4 ok），`return 0` 则泄漏它（r5 MemoryLeak）。
- 实现：
  - `Term::Return` 变体携带 `ret: Option<String>`（`#[serde(default)]`，
    旧 JSON 兼容）——MIR 经局部量 `_0` 返回，reg 由 `version.rs` 解析：
    多定义时按 reaching-definitions 取唯一版本（`return _0_vK`），两版本
    合流则置 `__VERSION_CONFLICT__` 哨兵（与 Drop 同款）；
  - `lower.rs`：非 void + 该 reg 在本块 `bound` 集内（支配门）→ 发
    `return <reg>`；void / 未绑定 / 冲突 → 回落 `return 0`，冲突另记
    `ReturnConflict` 大声（coverage 同构记 `T/ReturnConflict`）；
  - `drop.rs`：`return <reg>` 记为 move 源，出口释放不再重复释它
    （否则 UAM）。两个 drop 单测随之改口径（旧断言锁的是"return 后仍释放"，
    已被 r4 证伪）。
- 实测：corpus 6 个函数从 `return 0` 变为 `return _0`（含
  `f_opt_mutate` 返回 `Option::unwrap_or` 结果）；3 处合流冲突诚实大声；
  trap 谱不变（UAM 6 / Phi 2 / Leak 1）、全绿 31/40、parse-trap 归零；
  loud 104→107（+3 ReturnConflict，85.6%）；`cargo test` **58/58**（+2）。
- 意义：这是后端**调用约定的第一块真值**——此前任何非 void 调用的返回值
  都是 0；现在 extern 声明（STD_MAP 映射 sci/sa_std）的返回类型才真正有
  意义。残留：合流返回（需 phi）与 f_parse 的 `_0` 版本泄漏（同一族）。

## T20 rosetta 全量重跑（sci 318 + sla 296 文件，逐函数普查，量化 T18–T19b）

- 方法（口径对齐是关键）：新建批处理跑批（driver → mir2sa lower/coverage →
  逐函数切分 `sa check`，切分件带 `@import`+`@extern` 前导）跑
  `sci/demos/rosetta` 331 目录（318 产出 / 13 driver 拒收）与
  `sa_plugin_sla/demos/rosetta` 313 目录（296 / 17）；**基线用 git
  worktree 检出 T18 之前的 `09ba17b` 重新编译 mir2sa，同一 harness 重跑**，
  故 delta 可比（items 数完全一致：sci 6478、sla 9335）。
- 量化收益（基线→现状）：
  - sci：全绿函数 **391→426**（77.7%→84.7%，+35）；BorrowConflict 21→2；
    UseAfterMove 63→46；MemoryLeak 5→6；Phi 23→23；loud 805→830。
  - sla：全绿函数 **366→399**（71.9%→78.4%，+33）；BorrowConflict 27→5；
    UseAfterMove 73→61；MemoryLeak 20→21；Phi 23→23；loud 2291→2329。
  - 合计 **+68 个全绿函数**、Borrow **48→7**、UAM **136→107**。
  - loud 上升（+25/+38）主要是 T19b 的 `ReturnConflict` 具名记账
    （sci 23 / sla 36），属诚实口径而非新增缺口。
- 本轮抓出并修掉一处**我方新引入的 trap**（rosetta 重跑的价值所在）：
  sla `201_pkg_manifest_basic` 报 UnknownRegister——T19 的 cast 重载把
  `load _2_spill+0 as u8` 直接塞进 `zext ... as i32` 的操作数位，而 SA 的
  转换操作数必须是寄存器（嵌套表达式不解析）。修法：转换前先把重载绑到
  `_mv_{bb}_{i}` 临时量（`bind_move_operand` 同款），plain-copy 转换仍可
  内联 load。加锁单测 `cast_reload_binds_temp_for_convert`（**59/59**）。
  修后 sla UnknownRegister 归零（并揭出该文件 1 例旧泄漏，MemoryLeak 20→21）。
- 诚实记录：sla 的 `MemoryLeak` 20→21 与 sci 的 5→6 均为「被前序 trap 掩盖、
  修复后显形」的旧泄漏（Referee 首 trap 即停），非新增能力缺口。
- 工具沉淀：批处理与 census 脚本在 `scratch/`（已 gitignore），方法与探针
  结论均已抄入本节与源码注释。

## T21a 字节字面量：驱动补字节 + 值位内联（sci loud 807→510，sla 2293→2034）

- 缺口定性（`scratch/const_shapes.py` 全集普查常量形态）：rosetta 的
  ConstValue 头号形态是 **`&[u8; K]` 定长字节数组常量**（sci 约 300 项、
  sla 约 295 项），此前驱动只对 `&str` 放行字节、且后端只在**聚合字段位**
  处理字面量，值位（`_x = <const>`）一律大声。
- 驱动侧（tyinfo.rs `str_const_bytes` 放开）：`&[u8; N]`/`&[u8]`/`&u8` 与
  `&str` 同为字节载荷，一并下发 `str_bytes/str_len`；长度来源分两形——
  非定长（`str`/`[u8]`）取 `ConstValue::Slice { meta }`，定长（`[u8; N]`）
  是 `ConstValue::Scalar(Scalar::Ptr(..))`，取 `try_to_scalar_int()` 的
  `Err(Scalar<AllocId>)` 分支拿 AllocId、长度取 pointee 布局
  （`tcx.layout_of(PseudoCanonicalInput{..})`）；读取区间按分配实际大小夹紧
  （`get_bytes_unchecked` 信任区间）。不再要求 UTF-8（字节数组本就不是）。
- 后端侧（layout.rs `plan_const_bytes` + render.rs Use 臂）：值位定长字节
  数组内联为 `_str_{bb}_{i} = alloc K` + 逐字节 `store`，再把薄指针绑到
  dest。**只做定长**：`str`/`[u8]` 是胖指针（ptr+len），一个寄存器装不下，
  仍大声（聚合字段位走既有双 `store` 路径）。新增
  `const_needs_loud_value` 做**位置敏感**判据（值位可内联、其余位置沿用
  严格判据），lower/coverage 共用同一谓词，parity 成立。
- 回归修复（T21a 自身引入，rosetta 抓出）：值位常量转真值后，
  `190_base64_encode_simd` 报 UAM——`_1 = <字节常量>` 的两个 Copy 用仍被
  降成移动（T20 靠 constmap 把大声占位 `0` 重新物化而侥幸绕过）。修法：
  字节常量 dest 也按 `ptr` spill（值已知类型，Copy-uses 走重载），Use 臂
  补发槽位。加锁 2 用例（`byte_const_dest_spills_when_copied` /
  `fat_byte_const_dest_does_not_spill`），**61/61**。
- 实测（同驱动 A/B：T20 提交 vs 本轮）：
  - sci：loud **807→510**（cov 87.5%→**92.1%**），ConstValue 500→180，
    全绿函数 426 不变，trap 谱不变（UAM 46 / Phi 23 / Borrow 2 / Leak 6）。
  - sla：loud **2293→2034**（cov 75.4%→**78.2%**），ConstValue 1236→941，
    全绿 399 不变，UAM 修复后回到 61（与 T20 同）。
  - 累计（对 pre-T18 基线）：sci loud −295、sla −257；全绿 +35/+33。
- corpus 不变（本集无字节字面量常量），已重落确认 loud 107 / 85.6%。
- 下一步（T21b）：`CallConstValue`（sla 803 项）——调用实参位同样可内联
  字节缓冲并传薄指针，是当前最大单一缺口。

## T21b-1 胖指针 meta 读取（UnOp::PtrMetadata，sci 13 + sla 5 项归零）

- 定性：rosetta 的 `CallConstValue`（sla 803）主类是 **`&str` 胖指针实参**，
  定长 `&[u8; N]` 只占少数（3 例样本全为 str）。而 `sci/sa_std` 的字符串
  API 一律是**两寄存器胖指针 ABI**（`sa_json_stream_new(&json_bytes: ptr,
  len: u64)`、`sa_json_object_get_string(..., &key: ptr, key_len: u64, ...)`），
  即胖指针必须传 (ptr, u64)——探针 t1 证实该形状合法（仅剩探针固有的未用
  参数泄漏）。这意味着当前把 `&str` 形参声明成单个 `ptr` 是**真 ABI 缺陷**
  （既少传长度也与 sa_std 不匹配），T21b-2 要按 sa_std 约定端到端改造。
- 本轮先做前置小件：`UnOp::PtrMetadata` 之前一律大声（胖指针取长度没有
  指令）。按 slice.sal 的 (ptr,len) 布局，meta 恒在 +8，故精确落为
  `_x = load p+8 as u64`（Move 操作数先绑临时量）。探针 m1/m2 取证形状合法。
  加锁单测 `ptr_metadata_reads_offset_8`（**62/62**）。
- 实测：sci UnOp-PtrMetadata 13→0、sla 5→0；loud/全绿/trap 谱均不变
  （该 rvalue 原先只影响记账，不影响可汇编性——修正记账即 honesty）。
- 结论已写入 INVENTORY：T21b-2 需要 **driver + 后端协同**的胖指针 ABI
  改造（形参 1→2 展开、实参 1→2 展开、常量与局部两种来源），单点改动会引
  入 CapabilityMismatch，故排为独立一轮。

## T21b-2 胖指针 ABI 端到端改造：**实测净回归，已回滚**（诚实负结果）

- 改动（全部在 worktree 试做，已 `git checkout` 回滚，树为 T21b-1 状态）：
  驱动 `ty_is_fat_ptr`/`param_sa_types` 把胖形参展开为 `["ptr","u64"]`、
  `fn_fat_arg_indices` 下发胖实参下标；后端 `render_call_args` 在实参位
  内联字节缓冲并传 (buf, len)，`call_arg_needs_loud` 供 lower/coverage 共用。
- 实测（两集全量，逐函数 `sa check`）：
  - sci 全绿 **426→151**、sla **399→144**，新增 trap **CapabilityMismatch
    306 / 303**；把「胖实参无法物化就整调用大声跳过」修掉后 mismatch 归零，
    但漏出 MemoryLeak 177/147、BorrowConflict 74/95（原先被 mismatch 首个
    报告掩盖）。
  - 根因（两层，都有实测支撑）：
    (1) **形参与实参必须同步展开**。`@extern` 声明来自驱动的展开 sig，而
        本仓局部模型是**一局部一寄存器**——胖局部（`&str` 局部）根本无法
        表示，于是这些调用只能发 1 个实参，与 2 形参声明不符（正是
        CapabilityMismatch 的来源）。「发调用但填 0 占位」也试过：arity 对了，
        但漏出的泄漏/借用冲突依旧（那是这些函数里**一直存在**、此前被
        mismatch 掩盖的真 trap）。
    (2) 因此本轮改造的**前置条件是「胖局部可表示」**（局部能持 (ptr,len) 对），
        单点 ABI 改造不成立。
- 定性留档：单 `ptr` 声明胖形参**确是 ABI 缺陷**（与 sa_std 的
  `&bytes: ptr, len: u64` 不一致，少传长度），但修它需要先做胖局部表示
  （局部槽持对 + 读取 meta），属独立一轮的地基工作。sla 的 CallConstValue
  803 项因此**保持大声**，不假装可修。
- 教训（已写进流程）：ABI 类改动必须「声明端 + 调用端 + 数据表示」三者同时
  到位，否则一律净回归；先量后改，别先改后量。

## T22 缺口普查定性：**胖指针局部表示是唯一最大前置**（sla 剩余 loud 的 ~86%）

- 普查方法：对两集重新生成 mir.json，把所有常量按「形态 × 位置」分类
  （`scratch/const_shapes.py`）。
- 结论（T21a/T21b 之后 sla 余量 2029 的构成）：
  - `&str` **胖指针**常量：值位（`Use`，`Val(Slice{..}, &'{erased} str)`，
    字节已下发但一个寄存器装不下 ptr+len）≈ **941**，调用实参位
    （`CallConstValue`）≈ **803**——两者都是胖指针，合计 ~1744，占 sla
    loud 的 **86%**。
  - 定长 `&[u8; N]`（薄指针）已在 T21a 全部落地（值位 138+ 处不再计缺口）。
  - 剩余零星：`Unevaluated`（驱动 const-eval 可解，量级 ~30-45）、
    `Ty(usize, N)`（Repeat 长度）、`FnDef` ZST。
- 因此下一轮的**地基**明确为「胖指针局部表示」，它一次性解锁：
  1. 值位 `&str` 常量 → 缓冲 + `_N = buf` / `_N_len = K`（双寄存器局部）；
  2. 调用 ABI 展开（T21b-2 已原型化并实测：形参 1→2 + 实参 1→2 同步展开，
     本轮实测的 306/303 CapabilityMismatch 正是缺这层数据表示）；
  3. 胖局部作为实参（今天必大声，因为只有一寄存器）。
- 前置依赖清单（下一轮开工前需齐备）：
  - 驱动：下发**局部胖性**（当前 `Function.locals` 为空数组，类型信息没进
    JSON）——schema 加 `fat_locals` 或 locals 类型；
  - 后端：双寄存器局部命名（`_N` / `_N_len`）、版本化/支配集/spill 三处
    按「一个 MIR 局部 = 两个 SA 寄存器」对齐（version.rs、order.rs 的
    支配种子、spill.rs 的槽位都要知道 `_N_len` 独立存在）；
  - 聚合字段侧胖指针**已可用**（(ptr,len) 双 store，T12 起），无需改动。
- 该结论已写入 INVENTORY 的「出路」列，后续排期以它为准。

## T23 胖指针局部表示：**可实现但暂不 ship**（+ 挖出并修掉一个既有 bug）

- 方案（比 T22 设想的「双寄存器局部」更小）：胖值用**长度头缓冲**表示
  ——`[len: u64][payload: u8..]`，局部只存缓冲地址（仍是「一局部一寄存器」），
  需要长度的地方（按普查就是调用边界）用 `load p+0 as u64` 导出。这样
  version.rs / dom_seeds / spill.rs **零改动**。
- 实现（已试做并测量，随后回滚）：驱动 `ty_is_fat_ptr`/`param_sa_types`/
  `fn_fat_arg_indices`（形参展开 `[ptr,u64]` + 下发胖实参下标）；后端
  `layout::fat_const_buffer`（头缓冲）、`plan_fat_const`（值位）、`render_call_args`
  （胖实参 1→2 展开）、`render_call_args_lenient`（不可解析常量退化为 0 但保留
  寄存器读取）、`call_arg_needs_loud`（lower/coverage 共用）。
- **T21b-2 的阻塞确实解除了**：全程 CapabilityMismatch **0**（声明端与调用端
  同步展开，arity 恒匹配），值位胖常量与胖实参都真正落地，loud 实降
  sci 497→463、sla 2029→1881。
- 但**净损**：全绿函数 sla 399→319（sci 426→425），MemoryLeak 21→101。原因
  是本仓的「无 phi」限制在**放大后**才显形：合流处的版本化局部
  （`_3_v0`/`_3_v1`）谁都不支配 join，drop.rs 按支配门不敢在出口释放
  （否则另一支未绑定 → UnknownRegister）。此前这些寄存器装的是 null 标记
  `0`，Referee 不计泄漏；一旦承载真实缓冲就变成可见泄漏。
- **判定**：不 ship 胖 ABI。解锁它需要**合流版本的出口释放能力**（join 状态
  对齐 / merge-slot 范式，见 sla 官方做法），这与 PhiStateConflict 46 项是
  同一个地基。两项合并为一轮更有意义。
- 本轮**保留**两个净收益修复（与胖 ABI 无关，独立测量）：
  1. **块范围记账 bug**（lower.rs）：loud 终结符分支的 `continue` 会跳过
     `block_ranges.push`，导致这些块的行**从未进入 drop.rs 的输入** → 其定义
     永不被出口释放。改为「延迟收尾」（下一块开始时关闭上一块，且在标签后
     立刻 open，`continue` 也不丢）。加锁单测
     `loud_call_block_defs_still_exit_freed`。实测：sla 全绿 **399→412**，
     MemoryLeak **21→8**，loud 不变，无新 trap。
  2. **死字面量存储**（spill::dead_locals + lower 跳过）：定义后无人读取的
     字节字面量不再物化（否则分配了没人释放的载荷）。同批实测已含在内。
- 累计（本轮对两集的净效果）：sla 全绿 399→**412**、MemoryLeak 21→**8**；
  sci 持平（426 / 6）；loud 两集均不变（497 / 2029）。

## T24a 合流安全的分支内释放：**实测净损，已回滚**（第二个诚实负结果）

- 方案：单后继块 B 末尾补 `!R`——R 只在 B 定义、非借用源、非移动/已释放、
  B 内定义点之后无提及、且 J=B 后继及其支配集内无提及。纯文本层（drop.rs
  新增 `join_frees`，复用 `dom_sets`），loud 全程不增。
- 三轮迭代的实测（sci / sla 全绿函数）：
  1. 首版 426→328 / 399→221：插入用了**升序**遍历，`range_by_orig` 失效，
     释放落到错块（UnknownRegister 86/153、FallthroughForbidden）。
  2. 改降序单遍：426→358 / 399→349（UnknownRegister 153→3），但
     UseAfterMove 46→115：分支内释放的寄存器仍在出口被释放（两集合都基于
     **插入前**文本计算）→ 双释。
  3. 交叉过滤（分支 vs 出口）+ 祖先/后代去重：426→423 / 412→410。
     MemoryLeak 6→2 / 8→3（**目标达成**），但 UseAfterMove 46→51 / 61→66
     且出现 2/2 UnknownRegister。
- 结论与教训：
  - 收益上限已被 T23 的块范围修复吃尽（可修的泄漏只剩 2+3 项），而文本层的
    「join 之后无读取」判据**无法覆盖所有路径**（dom_sets 是 MIR 级，发射文本
    还含块内标签边），残留 UAM 反而更多。
  - 这类「生命周期/合流」问题在文本层做不彻底，必须在 **MIR 层**做真 phi
    （T24b）：join 处为每对到达版本建合并槽，值在支配集对齐后再读出。
  - 判据升级：先量收益上限（此处 5 项泄漏）再动手；上限低于「新增 trap 的
    风险面」时不 ship。三次迭代的数据已留档，patch 存于 scratch。

## T26 checked 算术：`*WithOverflow` 226 项全部关闭（走 sa_std，不原创）

- 问题：`*WithOverflow` 产出 (值, 溢出标志) **对**，本仓一局部一寄存器装不下；
  此前整条 BinOp 大声（sci 110 / sla 116），且其后的
  `Assert(Overflow(..))` 拿**值寄存器**当标志去比较（语义错）。
- 关键取证：MIR 里 `*WithOverflow` 之后**总是**跟一个 `msg` 以 `Overflow(`
  开头的 `Assert`（`24_factorial` 等逐例确认，msg 形如
  `Overflow(Sub, copy _1, const 1_i32)`），即这对里的标志唯一消费者就是那个
  断言。于是「值」可独立交付：溢出检查由 sa_std 的 checked helper  trapping。
- **sa_std 侧正规补充**（遵守「禁止原创」）：新增 `sci/sa_std/num.sai` 声明
  `sa_num_add_checked / sub_checked / mul_checked`（i64 → i64），并在
  `sci/sa_std/num.sal` 落同款守卫宏 `NUM_ADD_CHECKED` / `NUM_SUB_CHECKED` /
  `NUM_MUL_CHECKED`（符号判定 + `panic(PANIC_ARITH_OVERFLOW)`，mul 用
  「乘回再除，无余数」判据）。探针取证 `gt/ge/lt/sub/mul/div/rem/ne/or/and/br/
  panic` 均为合法 SA 指令（`br` 必须是语句而非可赋值表达式——这一点曾让我的
  探针误报 ForbiddenSyntax）。
- 后端：`binop_mnemonic` 之前先查 `checked_arith_helper`，命中则发
  `dest = call @sa_num_*_checked(l, r)`；溢出 `Assert` 折叠为注释 + `jmp`
  （控制流保留），并**大声记账** `OverflowAssertFolded`（折叠是语义变换，
  必须可见）。prelude 增加 `@import "sa_std/num.sai"`。新增 2 个锁
  （`checked_arith_shape` / `overflow_assert_folded`，**65/65**）。
- 实测：`BinOp-*WithOverflow` **sci 110→0、sla 116→0**；折叠记账
  137/147；loud 497→520 / 2029→2054（折叠必须记账，故 loud 略升——这是
  口径的诚实代价，不是新增缺口）；全绿 426→425 / 412→410（−1/−2，来自分支
  内新暴露的 3 例**比较临时量**泄漏，与本改动无因果：`181_file_descriptor_raii`
  零 `sa_num_*` 调用，泄漏是 `_sw_bb5`）。
- 结论：**语义收益明确 ship**（此前该类算术产出的是垃圾值 + 垃圾比较，现在
  值精确、溢出按 Rust 语义 trapping），指标上的 loud 上升已在 INVENTORY 说明。
  残留比较临时量泄漏留给下一轮（针对性、可证明安全的窄口径修复）。

## T27 比较临时量的块内释放（窄口径，净正：green +2/+1，泄漏 −2/−1）

- 目标类：残留的 MemoryLeak 全是**我们自己合成的比较临时量**（`_as_*` 断言
  条件、`_sw_*` switch 条件及其 `_eq*`），每个 demo 恰好 1 项。它们定义在
  不支配出口的块里，`exit_frees` 的支配门不敢释放（如
  `129_seqlock_optimistic` 的 `_as_bb9`、`181_file_descriptor_raii` 的
  `_sw_bb5`）。
- 实现（`drop::temp_frees`，刻意窄）：只处理 `_as_*`/`_sw_*`；条件是
  (1) 定义在本块、(2) 从未被 `!` 释放、(3) **没有任何其他块提及**它、
  (4) **没有被任何 `br`/`return` 行读取**。
- 三轮迭代的实测（这是 T24a 的教训：门控必须覆盖「块内中部」的控制流）：
  1. 首版用「定义点之后无提及」→ 零效果（比较临时量**本来**就被自己的
     `eq` 读一次，那正是它的用途，不是活跃性）。
  2. 改为「跨块无提及」后：MemoryLeak −4/−6 ✓，但 **PhiStateConflict
     +5/+8**、green −1/−2——`Assert` 块里 `br` 在**块中部**（失败臂是紧随的
     标签，块末是 `panic`），所以「不在最后一行」拦不住被 `br` 消费的
     `_as_*_eq`。
  3. 改为「任何 `br`/`return` 行读到即跳过」→ **净正**：green 425→427 /
     410→411，MemoryLeak 7→5 / 10→9，Phi 回到 23/23（无新增）。
- 顺带实测并否决的扩展：把 `_mv_*`（移动绑定临时量）也纳入 → 立刻出现
  UseAfterMove（它的消费与自由交织），已排除并在注释里写明原因。
- 两个集合的过滤是**单向**的：先从出口列表里剔除块内释放的，再据此剔除
  块内列表里已在出口释放的。双向过滤会把同时命中两表的寄存器从两边都删掉
  （实测即「哪边都没释放」——一次已修的真实 bug）。
- 加锁 2 用例（`temp_frees_block_local_only` /
  `temp_frees_skips_cross_block_and_branch_reads`），**67/67**；corpus loud
  114 不变（语料无此类泄漏），全绿 31/40 不变。
