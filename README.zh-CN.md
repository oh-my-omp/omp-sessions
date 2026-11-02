# omp-sessions

[English](README.md) | 简体中文

读取本机的 omp 会话（`~/.omp/agent/sessions`）：`session://` scheme 让 `read` 与 `grep` 直接打开其他会话，不必手动定位 JSONL 文件；`session_query` 设备让 eval 单元用 JavaScript 和 SQL 查询同一份历史。

## 安装

```sh
omp install https://github.com/oh-my-omp/omp-sessions
```

入口是包根目录的 `index.ts`，由 `package.json` 的 `omp.extensions` 声明。插件没有设置项。SQL 引擎是 DuckDB（`@duckdb/node-api`，随插件安装的原生模块）。

## `session://`

只读的内部 URL scheme，注册在 omp 进程全局的 URL 路由里，主会话、子代理和轻量模式都能用。`read session://README` 返回完整语法。

- **查找**：`session://find?text=<词>[&project=.|<bucket>|<路径>][&max=N]` 匹配 prompt 历史（`history.db` 中 omp 的 FTS 索引）、标题和 recap；`session://sessions.jsonl`、`session://here/sessions.jsonl`、`session://projects/<bucket>/sessions.jsonl` 以 JSONL 列出会话元数据。
- **阅读**：`session://<id 前缀>/timeline` 渲染当前分支：用户、developer、thinking、assistant 正文、IRC、task 派发和子代理结果全文显示；其余工具调用每个一行 `tool(参数) 状态 #<结果条目> // 意图`。`…/entry/<id>` 完整显示一个条目；`…/events.jsonl` 是同一组事件的 JSONL；`…/raw.jsonl` 是文件本身；`…/agents/<AgentId>/…` 对子代理重复这套结构；`…/artifacts/<N>` 与 `…/local/<路径>` 是该会话的 `artifact://` 与 `local://`。
- **过滤**：`timeline` 和 `events.jsonl` 接受 `?type=<filter>`（事件类型、分组、`call:<工具>`、`marker:<类型>`、排除写法）和 `?since=…&until=…` 时间窗；grep 带 glob 时用路径前缀 `session://type/<filter>/<路径>`。`events.jsonl`、`sessions.jsonl` 与 `find` 接受 `?q=` 执行 jq。
- **grep**：只有一棵可遍历的树 `session://projects/<bucket>/<YYYY-MM-DD>/<完整 id>/timeline`（及 `…/agents/<AgentId>/timeline`），当前项目可写作 `session://here/…`。每次读取用投影式 tokenizer（`src/scanner.ts`）流式扫一遍文件，不做缓存。

## `session_query`

`xd://` 设备。eval 单元以 `(await tool.session_query({ action, … })).details` 调用，JavaScript 与 Python 都可以。

| action | 结果 |
|---|---|
| `sessions` | 会话元数据行，可按项目、关键词和 `modified` 时间窗过滤 |
| `agents` | 一个会话的转录树：根与每个子代理及其所属的分配 id |
| `events` | 一份转录的当前分支事件，分页返回 |
| `sql` | 对 `from` 点名的会话和 `tables` 传入的行执行 DuckDB SQL |
| `export` | 把一个查询结果写成调用方指定路径的 parquet、csv 或 json 文件 |

- **在线计算**：每次 `sql` 调用按点名的会话，从当时的会话文件建出 schema `src`，调用结束即删除。`src.entries` 直接在磁盘上原地读取每一行 JSONL；`src.events`、`src.transcripts`、`src.sessions` 来自与 `session://` 相同的加载器，因此按当前分支、用结果补全工具调用、带 recap。不保留任何会话副本。
- **中间结果**：在 schema `main` 里建的表、视图和宏保存在该会话的内存数据库里，后续调用可用；`/new`、resume、fork 或会话结束时清空。单元里的行经 `tables` 以 `src.<name>` 参与查询。
- **SQL 的边界**：允许查询和修改数据库自身的对象；COPY、EXPORT、ATTACH、INSTALL、LOAD、SET、PRAGMA、CALL 等引擎或文件语句一律拒绝。SQL 只能读会话目录下的文件，不能写任何文件；`export` 才写文件，从不写进会话目录，并申请 write 审批。
- 调用约定见设备专题 `xd://session_query/actions` 与 `xd://session_query/sql`（`src/query/topics/`）。

工具标签跟随界面语言：`OMOMP_LANG`（`en` 或 `zh`），其次是 agent 目录下唯一的 `omomp.{yml,yaml,json,toml}` 的 `omomp-lang` 段，再次是 `LC_ALL` / `LC_MESSAGES` / `LANG`，最后是英文。模型读的说明、专题和结果保持英文。

## 宿主耦合

所有 omp 内部 import（URL 路由、会话列表、索引与加载辅助函数、`tools/read-json`）集中在 `src/omp-adapter.ts`；omp 升级挪动其中任何一个时在加载时报错。scheme 处理器不覆盖 omp 自带的 `session` scheme 或其他宿主的处理器。

## 开发

```sh
bun install
bun run check
bun test
```

本地运行时直接加载入口：`omp -e ./index.ts`。
