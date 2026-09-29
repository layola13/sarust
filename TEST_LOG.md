# sa_plugin_rsc 验证记录 (2026-09-29, 容器实跑, 100% Rust / 0 Python)

> 全支持路线：`INVENTORY.md` 为总表；语料库 `corpus/`（24 fns + closures +
> consts + statics）`mir2sa coverage` **98.4%**，demo 工程 `--strict` 全绿
> （`UNSUPPORTED=0`）。落法已对齐 `sa_plugin_sla`（`@extern` 闭包、`&`/`^`
> 前缀、`!` 释放、`alloc`+`store` 数组、`sa_mem_set` 复写）。

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
语料库全量：245 / 69 / 29 全等 → **CORPUS_FIDELITY_OK**。
每个 SA 所有权标记都 traced 到一条真实 MIR 事实。

## T4 轻文本兜底（mir2sa parse，不链 rustc_private）

`mir2sa parse main.mir.txt --fn main`：33 blocks / 41 stmts / unsupported=0，
与已删 Python 解析器输出 JSON_EQUAL（除 1 处 Python 把 `otherwise` 误收进
targets 的 bug，Rust 版已修正；另 `(_4.0: T)` 投影归一到基 local）。

## T5 反例（大声失败）与单测

- 未知 stmt kind → `bad mir.json: unknown variant …`，`RC=2`。
- `--strict` 下有 UNSUPPORTED → `RC=1`。
- `hi.mir.json → hi.sa`：`UNSUPPORTED=0`。
- `cargo test` 3/3：`scalar_hex_driver_form`、`const_elem_both_forms`、
  `array_init_bb30_shape`（array-init 回归锁）。

## 剩余缺口（诚实）

1. `cargo build rsc_driver` 不可行（cargo 不解析 sysroot crate），固定走 `build.sh`。
2. `&mut` Phase1 降级为 `&` + Referee（与 `sa_plugin_sla` 已知局限一致）。
3. `alloc <数字>` 直接量与 `store` 元素类型写法待 `sa` 汇编器到货后做汇编级校验
   （当前以 `sci/sa_std/alloc/vec.sa` 现行写法为对齐依据）。
