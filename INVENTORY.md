# MIR 全 kind 清单与支持状态 (nightly-1.101 c1070d693 + driver 实测闭环)

> 基线：语料库 `corpus/`（24 fns + closures + consts + statics，428 stmts + 317 terms）
> `mir2sa coverage` = **85.6%**（107 缺口，全部具名；见末尾）。
> 验收集（T20 起为逐函数 `sa check` 普查口径）：`sci/demos/rosetta`
> 318 文件/503 函数/6478 项（loud 510，cov 92.1%）+
> `sa_plugin_sla/demos/rosetta` 296 文件/509 函数/9335 项（loud 2034，cov 78.2%）。
> 两仓官方 rustc 拒收的非独立 demo（外部 crate/未完成 nightly
> 特性/缺构建产物）逐项定性（见 TEST_LOG T13），不计入。
> driver 见到即命名，绝不静默吞掉。
>
> 口径说明（T14 起）：`coverage` 只数 MIR-kind 可判定项的时代结束。
> `sa check`（汇编器）为准绳：凡发射行不能过 flatten/parse（ForbiddenSyntax、
> UnknownRegister、CapabilityMismatch、IllegalUnsafeContext、UnsupportedType），
> 一律大声计数——即使 MIR kind 已知。当前三集 parse-trap 归零（逐函数普查），
> 剩余 trap 全部是 Referee 层（仿射/借用/泄漏，见 TEST_LOG T14），属下一阶段。

## 模块地图（AGENTS.md 分模块规则）

| 模块 | 职责 | 行数 |
|---|---|---|
| `driver/util.rs` | 文本工具（esc/trunc/sanitize/bb_name/local_name） | ~50 |
| `driver/place.rs` | place 基 local 归一 + via 原文 | ~30 |
| `driver/tyinfo.rs` | 类型布局与签名（layout_of/for_variant/签名映射/str 取字节/ZST 判定） | ~200 |
| `driver/emit.rs` | mir.json 发射（operand/rvalue/terminator/body） | ~470 |
| `mir.rs` | mir.json schema（Operand/Rvalue/Stmt/Term/签名/布局） | ~170 |
| `parse.rs` | `-Zunpretty` 文本 → mir.json + kind 分类 | ~420 |
| `render_util.rs` | 标识符/标签/十进制化/门控谓词（loud 判定） | ~200 |
| `render.rs` | Rvalue → SA 行（调用/二元/转换/聚合/Discriminant/RawPtr/Repeat/TLS） | ~170 |
| `layout.rs` | 数组/Adt 物化（alloc+store）+ FNV/field 计划 | ~220 |
| `asm.rs` | asm 门控（mov/inout）+ cast 决策 + 标量宽度 | ~210 |
| `order.rs` | RPO 排放序 + 支配集 bound 种子 | ~230 |
| `version.rs` | SSA 版本化（重命名+reaching-definitions+冲突哨兵） | ~630 |
| `borrow_end.rs` | Drop 点借用终结 + cleanup 去重（支配门控+可达性+单定义+死后无用；T18/T18b） | ~470 |
| `lower.rs` | 函数装配（头/块/终结符/extern/占位/重绑定） | ~440 |
| `drop.rs` | 出口释放插入（支配感知+借用拓扑） | ~340 |
| `spill.rs` | 多用值 reload 槽（call/合成/cast/借用 dest；T19 扩类） | ~370 |
| `main.rs` | CLI + coverage 镜像 + 单测 | ~700（含单测；逻辑约 400） |

## StatementKind

| kind | 状态 | SA 落法 |
|---|---|---|
| Assign | ✅ loud-完备 | 按 Rvalue 表；重绑定/不可判定常量走占位+计数 |
| StorageLive / StorageDead | ✅ | `//` 注释（栈槽标记） |
| Nop / ConstEvalCounter / Coverage | ✅ | `// nop:` 注释，不计数 |
| FakeRead | ➖ | 未在 optimized MIR 出现；出现即 UnsupportedStmt |
| PlaceMention | ➖ | 同上 |
| AscribeUserType | ➖ | 同上（借用检查后无运行时意义） |
| SetDiscriminant | ✅ | `store base+0, variant as i64`（sla `enum_tag_offset=0`；语料库 2 处已落） |
| Intrinsic | ➖ | 未出现；出现即 UnsupportedStmt |
| BackwardIncompatibleDropHint | ➖ | lint only；出现即 UnsupportedStmt |

## Rvalue

| kind | 状态 | SA 落法 |
|---|---|---|
| Use | ✅ | `=`（SA `=` 本身即 move；`^` 仅合法于 call 实参/store 值位） |
| Ref (Shared) | ✅ | `&`；Mut 降级 `&` + 注释（Phase1，与 sla 一致）；ZST 被借用 → `= 0`（无存储 exact） |
| BinaryOp（符号无关子集） | ✅/🔶 | Add/Sub/Mul/BitAnd/BitOr/BitXor/Shl/Eq/Ne → 同名小写指令；`*WithOverflow` → T26 走 `sa_num_*_checked`（值精确、溢出 trapping，标志对不可表示）；有序比较、Shr 等需符号性 → 大声（Move 操作数先绑临时） |
| UnOp | ✅/🔶 | Not→`not`、Neg→`neg`；PtrMetadata 等 → 大声 |
| Cast | ✅/🔶 | kind+源类型双定：同宽/指针恒等→plain copy；变宽按符号 `sext/zext/trunc`；float 交叉 `fptosi/sitofp/uitofp`；Unsize/fn-ptr → 大声 |
| Discriminant | ✅ | `load place+0 as i64`（与 SetDisc 对偶；旧 `discriminant()` 伪指令非法已删） |
| Aggregate 零元素 | ✅ | `dest = 0`（unit/niche；tag 另由 SetDisc 写入） |
| Aggregate 单元素 | ✅ | 直接赋值（exact） |
| Aggregate 数组 `[T; N]` 全 Const | ✅ | `alloc` + `store`（sla/vec.sa 惯例） |
| Aggregate struct/tuple/range/enum | ✅ | `alloc` + 逐字段 `store`（p_layout v2：driver 下发真 `size/offsets`，`dest = _agg_bbN`；缺布局回退 v1 sla ABI） |
| Aggregate ZST 字段（PhantomData/Pinned） | ✅ | 占 0 字节，不发射 `store`（exact；全 ZST 则 `dest = 0`） |
| Aggregate 含 Slice/alloc 常量 | ✅ | driver 解析 `Const::Val(Slice)` 取真字节（`&str`+UTF-8 才下发 `str_bytes/str_len`，他形缺席保旧 fixture 字节兼容）；mir2sa ≤64B 内联字节缓冲 + (ptr,len) 双 `store`（slice.sal 布局）；超长/计数失配大声（`cargo test` 2 用例锁定） |
| Repeat `[c; N]` u8/i8 Const | ✅ | `alloc` + `call @sa_mem_set` + `dest = _rep` 绑定（旧版漏绑已修） |
| Repeat 其他 | 🔶 | 具名 + 计数（非常量元素需循环） |
| RawPtr | ✅ | thin 指针 plain copy（exact；`*p` 在非 ffi 上下文非法已删） |
| ThreadLocalRef | ✅ | `dest = call @sa_thread_local_slot(FNV1a(DefPath))`（注册表在 `sci/sa_std/thread_local.sai`，真 per-thread 隔离；语料库 2 处已落） |
| CopyForDeref | ➖ | 未出现；出现即 Unsupported（deref 语义待 p_layout 的 Deref 投影） |
| WrapUnsafeBinder / Reborrow | ➖ | 未出现；出现即 Unsupported |

## Operand / BorrowKind

| kind | 状态 |
|---|---|
| Copy / Move / Constant | ✅ |
| RuntimeChecks | 🔶 APPROX 计数（`0 /*RuntimeChecks-unsupported*/`，当前语料库 0 次） |
| BorrowKind::Shared / Mut / Fake | ✅（Fake 按 Shared 处理） |

## TerminatorKind

| kind | 状态 | SA 落法 |
|---|---|---|
| Goto / Return / Resume-Redirect | ✅ | `jmp` / `return <reg>`（T19b：经局部量 `_0` 返回，version.rs 解析 reaching 版本；void/未绑定/合流冲突回落 `return 0` + 大声）/ Resume→`panic` |
| Unreachable | ✅ | `panic(16xx)` 大声中止（裸 `unreachable` 会终结 SA 函数文本，禁排后继） |
| Call（含 diverging） | ✅/🔶 | 类型化 `@extern`（driver 下发 callee sig；`void` 调用裸写）+ `jmp`；无 sig/坏常量参数 → 大声保控制 |
| Drop | ✅ | `!p` |
| SwitchInt | ✅ | eq+双目标 br 链（行内 `==` 非法已删）；Move discriminant 单绑；RPO 保证文本序 |
| Assert | ✅ | 无 `assert` 指令：`eq`+`br`+数字 `panic`（复用 sa_core ASSERT_EQ 形）；expected 由 driver 下发 |
| UnwindResume | ✅ | `panic("unwind-resume")` |
| FalseEdge / FalseUnwind | ✅ | `Goto(real_target)`（语义即 goto） |
| TailCall | ➖ | 未出现（需 `become` nightly feature）；出现即 Unsupported |
| Yield / CoroutineDrop | ➖ | 语料库 async fn 未产生（被降解）；出现即 Unsupported，目标对接 `sa_std/libsa_async.sa` |
| UnwindTerminate | ➖ | 出现即 Unsupported |
| InlineAsm（纯 `mov` 拷贝形） | ✅ | driver 下发 `template/options/outs/ins`（span 已剥离）；`mov {0},{1}` + 空 options + 单 out/in → `dest = src` exact（Copy/Move 保持原样，`cargo test` 3 用例锁定） |
| InlineAsm（值稳定 `inout` 逃逸） | ✅ | driver 下发 `inout` + 双边（sla-117 形）；注释-only 模板 + 空 options + 单 out/in → 同 local 零指令（注释），分 local 补 `out = in` 拷贝；他形大声（`cargo test` 3 用例锁定） |
| InlineAsm（其他） | 🔶 | 具名 + 计数，大声 UNSUPPORTED（SA 无内联汇编；extern/intrinsic 策略 TBD；两仓 619 文件零残留） |

## SA 汇编器陷阱普查（`sa check`，T14–T20）

发射行 parse 层（ForbiddenSyntax/UnknownRegister/CapabilityMismatch/
IllegalUnsafeContext/UnsupportedType）：三集归零（逐函数/整文件普查）。
Referee 层由前端负责（sala 03 模型：Drop 插入与 Phi 由上游负责）：

rosetta 两列为 **T20 起逐函数普查**（同 harness、同文件集、同 item 数，基线
＝T18 之前的 `09ba17b` 版本 worktree 重跑；此前为整文件口径，不可直接比）。
基线→现状：sci 318 文件 503 函数 **391→426 全绿**（77.7%→84.7%）、Borrow
21→2、UAM 63→46、loud 805→**497**（cov 87.6%→**92.3%**）；sla 296 文件
509 函数 **366→399 全绿**（71.9%→78.4%）、Borrow 27→5、UAM 73→61、loud
2291→**2029**（cov 75.5%→**78.3%**）。sla 余量的 ~86% 是胖指针（值位 ~941 +
实参 ~803），**同一前置**：局部需能持 (ptr,len) 对（见 TEST_LOG T22）。

| trap | corpus 40fn | sci 318 文件/503fn | sla 296 文件/509fn | 出路 |
|---|---|---|---|---|
| PhiStateConflict | 2 | 23（23→23） | 23（23→23） | 合流/循环携带 Conflict，需 phi/merge-slot 范式 |
| UnknownRegister | 0 | 0 | 0 | T19 揭出「被移动 reg 上 `!r`」根因并修（借用 dest 保活）；T20 修「转换操作数内嵌 load」 |
| MemoryLeak | 1 | 6（5→6） | 8（20→21→8，T23 块范围修复） | `drop.rs` 出口释放（T17）+ 块范围记账修复（T23）；残量需 use-analysis + 合流版本释放 |
| UseAfterMove | 6 | 46（63→46） | 61（73→61） | const-prop + spill（T17）+ cast/借用/字节常量 dest reload（T19/T21a）；残留需 borrow-copy/use-analysis |
| BorrowConflict | 0 | 2（21→2） | 5（27→5） | borrow-end + cleanup 去重（T18/T18b） |
| RegisterRedefinition | 0 | 0 | 0 | 版本化+支配集重绑定检测已覆盖 |
| 全绿函数 | 31/40（3→23→28→31） | 426/503（391→426） | 412/509（366→399→412） | — |

仿射消费表（探针取证）：`x = y` 移动源；call/store/eq/load/br 共享读；
`&y` 锁定源（生借用未释禁 `!y`）；`!r` 释放；`^` 仅 call 实参/store 值位合法。
`drop.rs` 规则：单定义+支配出口+全程未移动/未释放+借用拓扑（借者先释），
不合一律跳过（宁漏不新 trap）。

## Place ProjectionElem（p_layout 主战场）

`Deref / Field / Index / ConstantIndex / Subslice / Downcast / OpaqueCast / UnwrapUnsafeBinder / PhantomDeref`：
driver 以 `p.local` 精确取基 local（替换旧 Debug 启发式；双括号形曾漏网出
`_proj` 未定义寄存器，已修；全集零 `_proj`）。读.f_struct/.f_slice 等因
optimized MIR 已把常用投影展开而恰好全过；v2（本轮）：driver 内 `place.ty()` +
`tcx.layout_of()`（`TypingEnv::fully_monomorphized`；枚举经
`AggregateKind::Adt` variant 走 `for_variant` 取 payload 布局；Primitive/
Union/失配一律回退，驱动永不因子布局失败）下发真 `size/offsets`，
mir2sa 覆盖偏移原文使用（reorder 非升序与枚举 tag-gap 绝对偏移皆 exact；
逐元渲染仍复用 v1：Const 按后缀十进制化，Move/Copy 作 u64 槽；
`total` 取 max(启发式, 真值）防缩水）。泛型单态（`f_generic`）无布局，
诚实回退 v1。

## 当前缺口（corpus 基线，T14 口径：汇编器为准）

`mir2sa coverage` **85.6%**（428 stmts + 317 terms，107 缺口，全部具名）：

| 类别 | 数 | 出路 |
|---|---|---|
| ConstValue（corpus 基线） | 38 | 驱动 const-eval（`const_eval_resolve`）；`&[u8;N]` 字节提升已于 T21a 落地（值位） |
| Rebind（同路重定义） | 23 | SSA 版本改写 + join-phi（Referee 程序） |
| `*WithOverflow` BinOp | 27（rosetta sci 110 / sla 116 → **0**） | **T26 已落地**：走 `sci/sa_std/num.sai` 的 `sa_num_*_checked`（值精确、溢出 trapping）；溢出 Assert 折叠并记 `OverflowAssertFolded`（折叠是语义变换，必须可见，故 loud 口径略升） |
| CallConstValue（调用实参） | 10 | **合流版本释放能力**是前置：胖指针值位/实参本身已可实现（长度头缓冲，T23 实测 arity 恒匹配、loud −148），但合流处的版本化局部谁都不支配 join、无法出口释放，放大后净损；需 join 状态对齐 / merge-slot（与 PhiStateConflict 同地基）。（`*WithOverflow` 已在 T26 用 sa_std checked helper 关闭） |
| 有序比较/移位（Lt/Gt/Ge/Shr） | 9 | driver 下发符号性 |
| PointerCoercion（Unsize/fn-ptr） | 5 | 胖指针构造/intrinsic 策略 |
| UnOp-PtrMetadata | 0（3→0，T21b-1 落为 `load p+8`） | 0（13→0） | 0（5→0） | 已落地：slice.sal (ptr,len) 布局 meta 恒在 +8 |
| FnSig（128 位） | 0（corpus） | 已有计数器；rosetta-09 触发 1 次 |

历史 100%（T13）为 MIR-kind 口径；T14 起以 `sa check` 为准绳，
上述缺口此前以不可汇编形态静默存在，现全部大声。`cargo test` 58/58；
`examples/corpus.{mir.json,sa,coverage.txt}` 为锁定产物。

## 保真（corpus 全量对账，T14 口径）

MIR 245 move / 69 borrow / 29 drop。SA 侧：rvalue 位 move 已改裸写
（`_x = ^_y` 非法；`=` 本身即 move），`^` 仅保留于 call 实参（已删：与
plain-param 声明 mismatch，改裸写）与 store 值位；`&`70（含合成借用）；
`!`29。`&(*_p)` 解引用再借用归基 local + `via` 原文。`examples/corpus.{mir.json,sa,co