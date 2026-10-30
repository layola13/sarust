# 驱动字段投影 / 偏移：发了什么、谁消费、off>0 覆盖

本文档复核自 field-precision 分支（`field-precision`，HEAD `24d6c34` +
fp-off0 跟进）的驱动代码与实测 mir.json / .sa，不转述上一任工人的结论。
上一任总结称“仅 mir2sa、2 提交”失实：分支实际 33 个文件，含驱动改动
`rsc/driver/emit.rs`、`rsc/driver/tyinfo.rs`。

## 1. 驱动侧发了什么

### 1.1 投影计算：`place_proj`（`rsc/driver/tyinfo.rs`）

驱动不再把每个 place 坍缩成根 local：`place_name`（`rsc/driver/place.rs`）
返回的 `place` 字符串永远只是根 local（如 `_1`），投影信息全部走下面
这些键。`place_proj` 沿 MIR 投影链、按 rustc layout 逐步累计：

- `off`：无 deref 时，是从根 buffer 到该字段的字节总偏移；有 deref 时，
  是从根 buffer 到“装着被解引用指针的那个 cell”的偏移（deref 在首位时为 0）。
- `off2`：deref 之后的尾随字段偏移；无 deref 时恒为 0。
- `deref` / `deref_first` / `deref_last`：链中恰好含一个 `Deref`，以及它
  是否在首位、是否在末位。
- `pty`：投影后位置的 SA 类型：标量名（`u64`、`i32`、`u8`、`f64`……）、
  引用/裸指针/函数指针记 `ptr`、聚合/数组/元组记 `buf`、零尺寸记 `zst`；
  旧形状或不可映射时为 `None`。
- `loud`（序列化为 `proj_loud`）：链无法精确解析时置位，mir2sa 必须走
  loud，绝不按根 local 渲染。置位情形：穿过一个 `buf` 类型字段后再继续
  投影（见 1.3）、`ConstantIndex`、动态 `Index`、subslice、opaque cast、
  多重 deref、layout 不可解、终类型不可映射。

无投影的 place 返回默认值：无偏移、无类型、不 loud，与旧行为一致。

### 1.2 序列化规则：`proj_json`（`rsc/driver/emit.rs`）

只发非默认值，缺键即默认值（serde default）：

- `off` / `off2` 为 0 时**省略**。所以“没有 `off` 键”不等于“没有投影”：
  field-0 读带有 `pty` 但没有 `off` 键；无投影的 place 则 `pty` 也没有。
- `deref` / `deref_first` / `deref_last` 为真时才发；`pty` 为 `Some` 时才发；
  `proj_loud` 为真时才发。

### 1.3 偏移≠源码字段顺序

偏移取自 rustc layout（`layout.fields.offset(i)`），而 rustc 的
repr(Rust) 会重排字段。实测两例：

- 08：`Outer { v: u64, inner: Inner }` 的 aggregate layout 是
  `{size: 24, offsets: [16, 0]}`——`inner` 被排到 0，`v` 被排到 16。
  于是 `o.inner` 这一步在 mir.json 里是 `off` 缺省（0）、`pty: buf`，
  而不是源码顺序以为的 8。
- 16：`H { t: u64, r: &P }` 的 layout 是 `{size: 16, offsets: [8, 0]}`——
  引用字段 `r` 被排到 0，`t` 被排到 8。于是 `h.r` 这一步是
  `pty: ptr` 且无 `off` 键（见第 5 节的修复）。

### 1.4 哪些位置发偏移、哪些不发

发（经 `operand_json` / `Ref` / `RawPtr` 分支调用 `proj_json`）：

- `Copy` / `Move` 操作数的一切位置：`Use`、BinOp、UnOp、Cast、Call 实参、
  Aggregate 元素、SwitchInt 判别式、Assert 条件、InlineAsm 输入。
- `Ref` / `RawPtr` 右值（借用地址本身带投影，见 2.3）。

不发（只有文本，没有结构化偏移）：

- Assign 的 `dest`：只有 `dest`（根 local）+ `dest_place`（MIR place 的
  Debug 文本，如 `(_1.1: u64)`）。字段 store 的偏移不由驱动给，
  由 mir2sa 从 `dest_place` 文本 + layout 索引解析（见 2.4）。
- `Drop` 的 place、`SetDisc` 的 place（`place` + `place_full` 文本）、
  `Discriminant` 的 place、Call 的 `dest`、Return（无 place）。

Aggregate 布局只发在**多操作数** aggregate 上（`layout: {size, offsets}`）；
单元素 aggregate 没有 layout 键。

## 2. mir2sa 侧如何消费

Schema 在 `rsc/mir2sa/src/mir.rs`：`Operand::Move/Copy` 与
`Rvalue::Ref/RawPtr` 携带 `off/off2/deref/deref_first/deref_last/pty/
proj_loud`，缺键时按旧行为解析。

### 2.1 判定：`render_util.rs`

- `operand_is_projected`：`off != 0 || deref || pty == "buf"` 时走字段
  load/add 路径，绝不按根 local 渲染。
- `operand_is_field_projected`：`off != 0 || pty == "buf"`，用于
  use-counting（spill 槽、borrow end、asm 门）。纯 deref 读（`*p`，
  off 0、标量/ptr 类型）不算 field 投影——313 曾因此丢槽。
- 标量 field-0 读被**故意**排除在投影之外：collapsed 的 overflow 对
  （`AddWithOverflow` 结果）不是 buffer，它的 `.0` 读持有值本身；
  03/05/06/10/12 里 `off` 缺省、`pty: u64` 的 bb1 读就是这种对子读，
  不是 buffer 字段读。

### 2.2 读值：`field_base` / `field_value_into` / `field_value_named`

- 无 deref：基址 = 根 local，偏移 = `off`。
- `deref_first`：基址 = 根 local（它已持有 pointee 地址），偏移 = `off2`。
- deref 在字段之后：先 `load 根+off as ptr` 得到指针，再用 `off2`。
- 标量按 `pty` load；`ptr` 按 `ptr` load；`buf` load 槽位里的指针，
  但 `deref && deref_last` 时值就是 deref 解析本身（整个 pointee
  buffer，如 10 的 Clone 式整结构 deref 拷贝），直接绑定不 load；
  `zst` 记 0。

### 2.3 取址：`field_addr_into`（`Ref` / `RawPtr`）

标量与引用字段的地址恒为 `add 基址, 偏移`——即使偏移为 0 也用 `add`
而不用 plain assign，否则会 move 掉基址指针；`buf` 字段的地址是 load
槽位指针；`deref_last` 时地址就是 deref 解析本身。

### 2.4 写：place store 不用驱动偏移

`place_store::plan_place_store` 解析 `dest_place` 文本
（`Field { field, .. }`），字段偏移查 `AdtLayoutIndex`：该索引全 crate
扫描 Aggregate init，按 `local_types` 里的类型名挂 layout；同一类型名
出现冲突 layout 时该条目作废、走 loud，绝不猜。基址必须是可证的
buffer 指针（引用类型 local、ptr 参数、多元素 Aggregate 定义）；
phi 托管的基址走 loud。值侧若是投影操作数，先经 `field_value_temp`
物化成临时变量再 store（18 的证据）。

### 2.5 field-0 规则：`lower.rs::field_zero_load`（192 + fp-off0 扩展）

Assign-Use 位置，`off` 为 0、无 deref 标志、`pty` 为标量**或 `ptr`**、
源 local 直接绑定 buffer（多元素 Aggregate、Repeat、buf 类型 Use 定义）
或为聚合类型参数时，渲染为 `dest = load 基址+0 as <pty>`，而不是
plain move。plain move 会绑定 buffer 指针本身（值错）并 consume 它
（后续字段读 UseAfterMove）。Collapsed 对（BinOp 定义）与 plain-Use
链不是 buffer，保持 plain 渲染。`ptr` 是 fp-off0 补的（第 5 节）。

### 2.6 loud 镜像

`rvalue_proj_loud`（驱动已标 loud，或投影了却无 `pty`）与
`rvalue_agg_field_copy_loud`（Use / 单元素 aggregate 里的 buf Copy，
即长寿命聚合字段拷贝；`deref_last` 的整 pointee 拷贝除外）由 lower
与 coverage 共用同一判定，计数不会漂移。

## 3. 老 13 个测试的 off>0 分类（逐个证据）

值口径：runner 的原生 exit 码 vs sa build-exe exit 码；“可区分”指
偏移丢了会改变 exit 码（读到 field-0 的值会不同）。

| 测试 | 判定值 | mir.json 证据 | .sa 证据 | 类别 |
|---|---|---|---|---|
| 01_struct_move_copy | `q.b`=22 | `Copy _1 off=8 pty=u64` | `load _1+8` | 测了 off>0，可区分 |
| 02_tuple_move | `u.2`=33 | `Copy _1 off=16 pty=u64` | `load _1+16` | 测了 off>0，可区分 |
| 03_field_binop | `p.b+p.c`=55 | `off=8` 与 `off=16` 各一 | 两次 field load 后相加 | 测了 off>0，可区分 |
| 04_field_callarg | `id(p.c)`=33 | Call 实参 `off=16 pty=u64` | 先 `_2 = load _1+16` 再 `call @id(_2)` | 测了 off>0（call 实参位），可区分 |
| 05_field_join | 应为 `q.b`=22，实得 0 | bb4 有 `Copy _4 off=8 pty=u64`，但 join 后该读被 version 冲突改写 | `_0 = 0` + `UNSUPPORTED ConstValue` | 测了 off>0 但失败，失败原因与偏移无关（join 冲突，见 6 节） |
| 06_field_loop | `p.b+i`=25 | bb4 `Copy _1 off=8 pty=u64` | `load _1+8` | 测了 off>0，可区分 |
| 07_field_param | `get_b(p)`=22 | `get_b` 体内 `Copy _1 off=8 pty=u64` | `load _1+8` | 测了 off>0（参数 buffer 字段读），可区分 |
| 08_field_agg_copy | `inn.y`=88 | 外层 `Copy _1 pty=buf` 且无 `off`（重排后 inner 在 0）；内层 `Copy _3 off=8 pty=u64` | `load _1+0 as ptr` 后 `load _3+8 as u64` | 部分：内层 off>0，外层只到 field-0（重排所致，非测试设计） |
| 09_field_ref_deref | `*r`=22（`r=&p.b`） | `Ref _1 off=8`；后续 `Copy _2 deref_first+deref_last pty=u64`（off2=0） | `_2 = add _1, 8` 后 `load _2+0` | 测了 off>0（借用地址位），可区分 |
| 10_field_aggcopy | `x.b+y.c`=55 | 整结构 deref 拷贝 `Copy _1 deref_first+deref_last pty=buf`；随后 `off=8`、`off=16` | buf 共享 + 两次 field load | 测了 off>0，可区分 |
| 11_field_store_read | `p.b`=99（先 `p.b=99`） | store 侧 `dest_place=(_1.1: u64)`（驱动无 off）；读侧 `Copy _1 off=8 pty=u64` | `store _1+8, 99` 后 `load _1+8` | 弱覆盖：只测存取偏移一致，不测绝对值——存取两边若同用偏移 0，值仍是 99 |
| 12_field_spill | `v+v`=44 | `Copy _1 off=8 pty=u64` | `load _1+8` | 测了 off>0，可区分 |
| 13_struct_update_spread | `q.b`=22（`..p` 展开） | spread aggregate 元素 `off=8`、`off=16`；终读 `Copy _2 off=8` | 元素先 `_fld = load _1+8/16` 再 store，终读 `load _2+8` | 测了 off>0（spread 元素位），可区分 |

结论：

- 只测 field-0 的测试：**没有**。13 个里没有任何一个以 buffer 的标量
  field-0 读为判定值（03 等的 off-0 读是 collapsed overflow 对，不是
  buffer 字段读；08 的外层 off-0 是 buf 槽位读）。
- `off2`（deref 后字段）：老 13 个里**一次都没有**（全 corpus grep 无
  `off2`）。09/10 的 deref 都是 `off2=0` 的纯 deref。
- store 绝对偏移、projected 值进 place store：老 13 个里没有
  （11 只读回同一字段，见上表弱覆盖）。

## 4. 新增测试 14–18（补齐未覆盖形态，全部值通过）

| 测试 | 补的形态 | mir.json 证据 | .sa 证据 | 结果 |
|---|---|---|---|---|
| 14_field_zero_read（`p.a`） | buffer 标量 field-0 读（`field_zero_load` 路径） | `Copy _1 pty=u64`，无 `off` 键 | `_0 = load _1+0 as u64` | 11=11 PASS |
| 15_field_ref_field（`r.c`，`r=&p`） | `deref_first + off2>0` | `Copy _2 off2=16 deref_first pty=u64` | `_0 = load _2+16 as u64` | 33=33 PASS |
| 16_field_holder_deref（`h.r.c`） | deref 在字段之后 + field-0 的 ptr 字段（重排后 `r` 在 0） | `Copy _2 pty=ptr` 无 `off`（H layout `[8, 0]`）；终读 `Copy _4 off2=16 deref_first pty=u64` | `_4 = load _2+0 as ptr` 后 `_0 = load _4+16 as u64` | 33=33 PASS（经第 5 节修复） |
| 17_field_store_neighbors（`p.b=99` 后 `p.a+p.b+p.c`） | store 绝对偏移 | store 侧 `dest_place=(_1.1: u64)`；读侧 `+0/+8/+16` 三次 load | `store _1+8, 99`；和为 143 | 143=143 PASS（存到 0 得 154，存到 16 得 132，只有 8 得 143） |
| 18_field_store_projval（`q.b = p.c` 后 `q.a+q.b+q.c`） | 投影值进 place store + 目标绝对偏移 | 值侧 `Copy _1 off=16`；store 侧 `dest_place=(_2.1: u64)` | `_3 = load _1+16`；`store _2+8, _3`；和为 37 | 37=37 PASS（值丢偏移得 15，目标丢偏移得 38） |

14/15/17/18 在修复前已一次通过；16 在修复前静默错值（见第 5 节）。

## 5. 修复：field-0 的 ptr 字段读（`rsc/mir2sa/`）

现象（修复前，16）：check 全过、UNSUPPORTED=0、值却是 0（原生 33）。
根因链：

1. rustc 把 `H { t, r }` 重排为 `[8, 0]`，`h.r` 的驱动投影是
   `pty=ptr` 且无 `off` 键（偏移 0 被省略）。
2. mir2sa 里 `operand_is_projected` 要求 `off != 0 || deref ||
   pty == "buf"`，`ptr` + off 0 不满足，走 plain 渲染 `_4 = _2`，
   把整个 H buffer 指针绑成了 `h.r`。
3. 192 的 `field_zero_load` 显式排除了 `pty == "ptr"`，只管标量，
   所以这一步没被截获；随后的 `_0 = load _4+16` 读的是 H buffer
   越界处（alloc 16，偏移 16），得 0，且无任何 loud——静默错值。

修复（`lower.rs::field_zero_load`）：去掉 `pty == "ptr"` 的排除，
`ptr` 与标量同走 `load 基址+0 as ptr`，门禁不变——仍要求源 local
在 `buf_def` 内或是聚合类型参数，collapsed 对与 plain-Use 链仍
plain 渲染。`scalar_width_bits("ptr")` 本就是 64，无需另改。
单测：`main.rs::field_zero_ptr_of_buffer_loads`（镜像
`field_zero_of_buffer_loads` 的 buf-def 结构）。

探针验证（未进语料，`/tmp/probe16x/`，已删证据链见汇报）：ptr
field-0 出现在 call 实参与 aggregate 元素位置时，优化后 MIR 都会
先把它物化成 Assign-Use 临时变量（如 16 的 `_4`），因此 Assign 位
的修复已覆盖这两个位置——两探针在修复后均为 33=33，且 .sa 里
`_4 = load _2+0 as ptr` / `_5 = load _2+0 as ptr` 均已出现。

## 6. 有意 loud 与仍未覆盖（不进值测试的原因）

- 直接嵌套读 `o.inner.y`：驱动 `place_proj` 在穿过 buf 类型字段后
  继续投影时置 `proj_loud`（mir2sa 的聚合字段是指针槽位，一次总
  偏移会读错 buffer），runner 口径下必 FAIL。正确写法是 08 的两步
  （先 `inn = o.inner` 再 `inn.y`）。此形态只记文档，不进语料。
- `ConstantIndex`、动态索引、subslice、多重 deref：驱动直接 loud，
  同上不进语料。
- 05_field_join：失败是 join 处 version 冲突（`_4` 两个 reaching
  定义 `_4_v0/_4_v1`，终读被改写为 Conflict → `_0 = 0` +
  `UNSUPPORTED ConstValue`），与偏移无关，不属 off0 lane，仍 FAIL。
- 残余（已观察、未证实可达）：若某天非优化 MIR 把 ptr field-0 投影
  直接留在 BinOp / Call / Aggregate 操作数里（不经 Assign 临时变量），
  render 层仍会 plain-move；优化后 MIR 未观察到此形状（见第 5 节
  探针），故只记此处，不另修、不进语料。

## 7. 门禁数字（fp-off0，修复后）

- 值测试：17/18 PASS，唯一 FAIL 是 05（第 6 节原因，基线起即 FAIL）。
- cargo：`cargo test --workspace`（`rsc/`）161/161 通过（含新增单测）。
- 全量扫表（`tools/sweep.sh --force`，332 文件，sa check 口径）：
  GREEN 209 / CHECK_ONLY 110 / TRAP 0 / SARS_FAIL 13，loud 256。
  与 `24d6c34` 基线（在 `sarust-fp-base` 工作区同法重跑）逐行
  diff：TSV 332 行全同，delta 为零。零 delta 是符合预期的：
  扫表只比对 check 与 loud 计数，不比对运行值；第 5 节修复改的是
  静默错值的渲染（plain move → load），既不增减 loud 也不改变
  check 结果，所以扫表上无表现；值的证明在值测试 16（33=33）。
  扫表后 T28 冻结文件（`tools/sweep-results-T28.tsv`、
  `tools/sweep-baseline-T28.md`）已 `git checkout` 恢复，`.bak` 已删，
  工作区只剩本节任务的改动。
