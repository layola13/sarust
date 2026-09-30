# MIR 全 kind 清单与支持状态 (inst nightly-1.101 + /content/rust checkout)

> 基线：语料库 `corpus/`（24 fns + closures + consts + statics，428 stmts + 317 terms）
> `mir2sa coverage` = **99.6%**，3 项具名缺口（见末尾）。driver 见到即命名，
> 绝不静默吞掉。

## StatementKind

| kind | 状态 | SA 落法 |
|---|---|---|
| Assign | ✅ | 按 Rvalue 表 |
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
| Use | ✅ | `=` / `^` |
| Ref (Shared) | ✅ | `&`；Mut 降级 `&` + 注释（Phase1，与 sla 一致） |
| BinaryOp (26 种全透传) | ✅ | `dest = Op(l, r)` |
| UnOp (Not/Neg/PtrMetadata) | ✅ | `dest = Op(x)` |
| Cast (12 种全透传) | ✅ | `dest = *op // cast: T`（取真目标类型） |
| Discriminant | ✅ | `discriminant(p)` |
| Aggregate 零元素 | ✅ | `dest = 0`（unit/niche；tag 另由 SetDisc 写入） |
| Aggregate 单元素 | ✅ | 直接赋值（exact） |
| Aggregate 数组 `[T; N]` 全 Const | ✅ | `alloc` + `store`（sla/vec.sa 惯例） |
| Aggregate struct/tuple/range/enum | ✅ | `alloc` + 逐字段 `store`（p_layout v1：sla tuple/struct ABI；Move 保持 `^` 可见；`dest = _agg_bbN`） |
| Repeat `[c; N]` u8/i8 Const | ✅ | `alloc` + `call @sa_mem_set` |
| Repeat 其他 | 🔶 | 具名 + 计数（非常量元素需循环） |
| RawPtr | ✅ | `*p // raw-ptr`（Referee: UnsafeBinder 语义） |
| ThreadLocalRef | 🔶 | 具名 + 计数，待 TLS runtime 设计 |
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
| Goto / Return / Unreachable | ✅ | `jmp` / `return 0` / `unreachable` |
| Call（含 diverging） | ✅ | `dest = call @f(args)` + `jmp`；callee 名经 `def_path_str` |
| Drop | ✅ | `!p` |
| SwitchInt | ✅ | Move discriminant 单绑 `_sw_bbN` 后扇出（修过 double-move） |
| Assert | ✅ | `assert` + `jmp` |
| UnwindResume | ✅ | `panic("unwind-resume")` |
| FalseEdge / FalseUnwind | ✅ | `Goto(real_target)`（语义即 goto） |
| TailCall | ➖ | 未出现（需 `become` nightly feature）；出现即 Unsupported |
| Yield / CoroutineDrop | ➖ | 语料库 async fn 未产生（被降解）；出现即 Unsupported，目标对接 `sa_std/libsa_async.sa` |
| UnwindTerminate | ➖ | 出现即 Unsupported |
| InlineAsm | 🔶 | `InlineAsm` 具名 + 计数（SA 无内联汇编；extern/intrinsic 策略 TBD） |

## Place ProjectionElem（p_layout 主战场）

`Deref / Field / Index / ConstantIndex / Subslice / Downcast / OpaqueCast / UnwrapUnsafeBinder / PhantomDeref`：
当前 driver 一律归一到基 local（`base_local`），读.f_struct/.f_slice 等因
optimized MIR 已把常用投影展开而恰好全过；v1 已用 sla ABI 启发式关闭
7 个 Aggregate + 2 个 SetDisc（`lower_adt_init` + `store tag+0`，`cargo test` 7/7 锁定）。
p_layout v2（真布局）：driver 内 `place.ty()` +
`tcx.layout_of()` 算出显式 `base+off`，随 mir.json 下发 `layout/offsets/tys`，
Adt 构造逐字段 `store`（Move 元素保持
`^` 可见），Downcast/niche 布局按真实 Layout 不猜。

## 当前 3 项缺口（corpus 基线，全部具名可复现）

- ThreadLocal ×2（TLS 静态初始化体）：待 TLS runtime 设计
- InlineAsm ×1（f_asm）：SA 无等价物，策略 TBD

已关闭（p_layout v1，本轮）：Aggregate struct/tuple/range ×7 +
SetDisc ×2，另 0-elem Aggregate（unit/niche）3 处一并 exact 化。
`cargo test` 7/7；`mir2sa coverage` 99.6%（428 stmts + 317 terms）。

## 保真（corpus 全量对账）

MIR 245 move / 69 borrow / 29 drop == SA `^`245 / `&`70（含 1 处
`call @sa_mem_set(&_rep_…)` 合成借用，无 MIR 对应，历史文件亦同）/ `!`29
（逐 place multiset 全等）。`&(*_p)` 解引用再借用归基 local + `via` 原文，
零 `_proj` 残留。`examples/corpus.{mir.json,sa,coverage.txt}` 为锁定产物。
