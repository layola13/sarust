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
