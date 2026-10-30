# off0 §6 残余风险核查（W4，2026-10-30，worktree gap/off6-risk）

结论：**不可达（no-op）**。当前驱动下，ptr field-0 投影从不直接出现在
BinOp / Call / Aggregate 操作数里；它总是在优化后 MIR 里先被物化成
Assign-Use 临时变量，而该位置已有 `field_zero_load`（标量与 ptr 同走
`load 基址+0 as ptr`）覆盖。因此不改渲染逻辑，不进 fieldprec 语料。

## 1. “非优化 MIR”在此语境的确切含义

驱动 `rsc/driver/driver.rs`（`after_analysis`）对全部非常量体取
`tcx.optimized_mir(did)`，仅常量/静态量走 `tcx.mir_for_ctfe(did)`。
管线输入永远是优化后 MIR；不存在取未优化 MIR 的分支。
§6 的“若某天非优化 MIR ……”只能指一种假设的未来变更：驱动改取
优化前 MIR 查询，或 MIR 构造/优化不再为字段读引入 Assign-Use 临时
变量。当前代码下该条件不成立，本核查即证伪其可达性。

## 2. 实证：全部探针均物化，直接形状数为 0

Holder 统一用 `H { t: u64, r: &P }`（rustc repr(Rust) 重排为
`[8, 0]`，即 `r` 在偏移 0）或等价的 repr(C) 原始指针版；
`P { a: 11, b: 22, c: 33 }`，判定值 33（`r.c` / 指针相等）。
每探针均跑 `sars -> mir.json -> mir2sa`，并用 `check.py` 扫描：
直接形状 = BinOp 操作数 / Call 实参 / Aggregate 元素中带有 `pty` 键
的 Copy/Move；物化形状 = `Copy <holder> pty=ptr`（无 off/off2/deref）
的 Assign-Use。

| 探针 | 类别 | 物化证据（mir.json） | 直接形状 |
|---|---|---|---|
| p_call_local | Call，局部 holder | `_4 = Copy _2 pty=ptr`，Call 实参 `Copy _4`（无投影） | 0 |
| p_call_param | Call，参数 holder | `_2 = Copy _1 pty=ptr`，Call 实参 `Copy _2` | 0 |
| p_call_mk | Call，函数返回 holder | `_4 = Copy _2 pty=ptr`，Call 实参 `Copy _4` | 0 |
| p_call_branch | Call，分支 join 后 holder | bb3 `_6 = Copy _3 pty=ptr`，Call 实参 `Copy _6` | 0 |
| p_agg_local | Aggregate，局部 holder | `_5 = Copy _2 pty=ptr`，Q 元素 `Copy _5` | 0 |
| p_agg_param | Aggregate，参数 holder | `_3 = Copy _1 pty=ptr`，Q 元素 `Copy _3` | 0 |
| p_agg_mk | Aggregate，返回 holder | `_5 = Copy _2 pty=ptr`，Q 元素 `Copy _5` | 0 |
| p_agg_tuple | Aggregate，元组 | `_5 = Copy _2 pty=ptr`，元组元素 `Copy _5` | 0 |
| p_agg_return | Aggregate，返回构造 | mkq 内 `_2 = Copy _1 pty=ptr`，Q 元素 `Copy _2` | 0 |
| p_binop_rawc | BinOp Eq，repr(C) 原始指针局部 holder | `_6 = Copy _2 pty=ptr`，BinOp 操作数 `Move _6` | 0 |
| p_binop_rawc_param | BinOp Eq，repr(C) 原始指针参数 holder | `_4 = Copy _1 pty=ptr`，BinOp 操作数 `Move _4` | 0 |
| p_binop_rawc_mk | BinOp Eq，repr(C) 原始指针返回 holder | `_6 = Copy _2 pty=ptr`，BinOp 操作数 `Move _6` | 0 |
| p_binop_fnptr | BinOp Eq，函数指针字段（repr(C) 首字段） | `_4 = Copy _1 pty=ptr`，BinOp 操作数 `Move _4` | 0 |

对照：标量 field-0（`p.a + p.c`、`id(p.a)`）与 off>0（03、04）同样
全部物化；04 当年文档记的“Call 实参 off=16”在当前驱动下实为
`_2 = Copy _1 off=16` 后 `Call(id, Move _2)`，与物化结论一致。

BinOp 探针选型说明：
- `&P == &P` 在 Rust 里不合法（E0369，无 PartialEq），故不用引用
  相等做 BinOp 探针；改用原始指针 Eq 与函数指针 Eq。
- repr(Rust) 原始指针版 `H { t, r: *const P }` 的布局是 `[0, 8]`
  （`r` 在 8，不是 field-0），与引用版 `[8, 0]` 不同；故原始指针
  探针用 `#[repr(C)] struct H { r: *const P, t: u64 }` 强制 `r` 在 0。

真实管线值（零参探针，transform + sa build-exe）：
`p_call_local` 33=33，`p_agg_local` 33=33，`p_binop_rawc_param`
经参数 wrapper（见下）33=33。

## 3. 反事实：若直接形状出现，渲染确实会 plain-move 错值

为区分“洞不存在”与“洞存在但不可达”，把真实 mir.json 的操作数
手工改成直连 ptr field-0（其余不动），再跑 mir2sa + sa：

| 手写直连 | .sa 证据（plain-move） | UNSUPPORTED | sa 运行 vs 原生 |
|---|---|---|---|
| Call 实参 `Copy _2 pty=ptr` | `_0 = call @idp(_2)`（holder buffer 直传） | 0 | 0 vs 33，静默错值 |
| Aggregate 元素 `Copy _2 pty=ptr` | `store _agg_bb0+0, _2 as u64`（holder buffer 存入 Q.r 槽） | 0 | 0 vs 33，静默错值 |
| BinOp 左操作数 `Copy _1 pty=ptr`（参数版，wrapper 供参） | `_3 = eq _1, _2`（holder buffer 与 q 比较） | 0 | 0 vs 33，静默错值；真实版 `_4 = load _1+0 as ptr` 后比较得 33 |

即：渲染洞是真实的（`operand_is_projected` 排除 off-0 ptr，
`field_zero_load` 只管 Assign-Use），但优化后 MIR 不产生该输入，
故按 §6 原定“只记文档、不另修”结案。若未来驱动改取未优化 MIR，
须先重跑 `run_off6.sh`：其形状断言会失败（check.py exit 1），
届时再修渲染层（把 field-0 ptr 判定推广到 BinOp/Call/Aggregate
操作数位），而不是现在为“可能”做重构。

## 4. 已知无关干扰（不属于本残余项）

- 零参原始指针 BinOp 探针（p_binop_rawc）的值路径被独立的
  RawPtr 借用释放问题挡住：`sa check` 报 `!_4 UnknownRegister`
  （`_4 = &_1` 被 move 后仍被释放），真实探针与手写版同错，
  与 BinOp 操作数位无关；故 BinOp 值证据改用参数版 wrapper。
- p_binop_fnptr 的 mir2sa UNSUPPORTED=2 来自函数指针常量
  （FnDef Cast），与 field-0 无关；其形状证据（物化）仍有效。
- 本目录探针不进 `tools/fieldprec-corpus/`：参数版与 fieldprec
  runner 的零参 wrapper 不兼容，零参 BinOp 版又各有上述独立
  干扰；进语料会污染 runner 基线。回归以本目录 `run_off6.sh`
  的形状断言为准。

## 5. 回归用法

```
tools/off6-probes/run_off6.sh [out-dir]   # 默认 /tmp/off6-run
```

逐探针保存 sars / mir2sa 各自的退出码，`check.py` 断言直接形状
为 0 且至少存在一次 ptr field-0 物化；另对 `p_call_local`、
`p_agg_local` 做原生 vs sa 值比对（33=33）。任一断言失败即
非零退出，代表 §6 残余已变得可达。
