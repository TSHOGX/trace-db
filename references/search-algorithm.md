# TraceDB 检索算法设计

> 本文档讲 `trace-db search` 排序器*为什么这样设计*：设计目标、五段式 pipeline
> 架构、评分模型的数学形式、复杂度边界、可调参数与实测效果。它是 `README.md`
> "Search" 章节（讲*是什么*）的深入版。代码在 `src/cli.ts` 的 `cmdSearch`
> 及其辅助函数。
>
> TraceDB 使用项目内置的 `fts5-jieba` tokenizer。§2.1 的 planner 与 §2.3
> 的 recency 分量也适用于扁平文档；②有界候选 cap / ④血缘 collapse /
> ⑤上下文装配则是本工具特有的会话结构。

## 1. 目标：这是情景记忆召回，不是通用文档检索

TraceDB 索引的是**五个 AI-CLI（Claude Code / Codex / OpenCode / Gemini / Pi）
的全部本地会话**——约 2500 个 session、39 万条 event、~0.8 GB `trace.db`。
用户的查询几乎总是"情景式"的：

- "今天 Claude 在 netlify 上部署那个页面是怎么弄的"
- "上周那次统计 opencode token 的 Codex run"
- "我们讨论 BM25 检索方案的那次会话"

这类查询的成功标准和通用 IR（"给定 query 返回最相关的文档"）不同，排序器就是
围绕这五点设计的：

1. **要的是"哪次会话"，不是"哪条 event"**——命中点只是线索，用户真正想要的
   是能定位、能一眼认出的**整个会话单元**。
2. **时间是强先验**——"最近那次"远比"三个月前那次"可能是答案；纯 bm25 无视
   时间。
3. **命中位置有语义权重**——命中在 `user` turn（人类的诉求原文）里，比埋在某个
   `tool_call` 的参数 JSON 里，更能说明"这次会话就是在讲这个主题"。
4. **一次工作可能横跨多个 session**——fork / subagent 把一份逻辑工作拆成多个
   session 文件，检索时不该让它们互相竞争、稀释排名。
5. **判断相关性需要上下文**——一条 12-token 的 snippet 不足以让用户确认"对，
   就是这次"；需要诉求、结论、命中前后文一起呈现。

## 2. 架构：五段式 pipeline

排序器是一条五段流水线，**纯机械、无 LLM、全部 query-side**（不改 schema，
不需要 `rm trace.db && ingest` 重建）。数据流：

```
query ──▶ ①Query Planner ──▶ FTS5 MATCH 表达式
                                    │
                                    ▼
        ②候选生成（event 级，per-session≤50 / total≤5000 封顶）
                                    │
                                    ▼
        ③session 级聚合打分（normBest + coverage + kind + recency + title）
                                    │
                                    ▼
        ④血缘 collapse（tree B：parent/fork → root，代表 + related[]）
                                    │
                                    ▼
        ⑤上下文装配（2 条批量 SQL：主命中±5 + 次命中簇 + ask/outcome bookend）
                                    │
                                    ▼
                             top-N 结果（每条自带足够上下文判断相关性）
```

### 2.1 ①Query Planner — OR-of-phrases

核心思路：**用 OR 把"精度臂"和"召回臂"并联**，让 bm25 自己在两者间权衡。

```
planFts("netlify deploy error")
  = "netlify deploy error"  OR  ("netlify" AND "deploy" AND "error")
     └─────── 精度臂 ───────┘     └──────── 召回臂 ──────────┘
```

- **召回臂** `("netlify" AND "deploy" AND "error")`：每个词单独引号 → jieba
  仍逐词分词/词干化，但三个词只要求**各自出现在同一 session 的某处**，不要求
  相邻。这是召回的主力。
- **精度臂** `"netlify deploy error"`：整串短语命中（三词相邻）时，该 event
  同时满足两个臂 → bm25 的 term frequency 更高 → **自然拿到更高分**，无需
  显式的 phrase 权重。精度是 bm25 的副产品，不是额外规则。
- **单 token** 退化为一个短语。
- **passthrough**：若查询里已含 FTS5 语法（`OR`/`AND`/`NOT`/`NEAR`/`"`/`*`/
  `(` `)` `:`），原样透传——power user 保留完全控制。判据是正则
  `FTS_OPERATOR`。

> 为什么不在 TS 侧自己分中文词再拼？因为 jieba 分词在 SQLite 扩展里，TS 侧
> 无法复刻。按空白切分对英文正确；对无空格的中文串，两个臂等价（整串即一个
> "词"），退化为单短语，也正确。把分词留给 tokenizer 是对的。

### 2.2 ②候选生成 — 有界扇出

```sql
WITH hits AS (
  SELECT ev.session_id, ev.id, ev.idx, ev.kind,
         bm25(events_fts) AS score,
         snippet(events_fts, 0, '«', '»', '…', 12) AS snippet
  FROM events_fts JOIN events ev ON ev.id = events_fts.rowid
  JOIN sessions s ON s.id = ev.session_id
  WHERE events_fts MATCH ? AND <过滤: agent/cwd/since/kind>
),
ranked AS (SELECT *, ROW_NUMBER() OVER (PARTITION BY session_id
                                        ORDER BY score ASC) rn FROM hits)
SELECT * FROM ranked WHERE rn <= 50 ORDER BY score ASC LIMIT 5000
```

- `bm25()` / `snippet()` 是 FTS 辅助函数，**过不了 GROUP BY**，所以这里保留
  event 级原始行，聚合放到 TS 侧做（§2.3）——kind 加权也因此落在 TS 侧
  （bm25 单列，无法在索引内按 kind 加权）。
- **两级封顶**：每 session ≤50 命中（`PER_SESSION_HIT_CAP`）、总 ≤5000
  （`CANDIDATE_HIT_CAP`）。防止 `the`/`的` 这类停用词式查询把整库拖进内存。
  bm25 升序 + LIMIT 保证留下的是全局最强的那批。

### 2.3 ③session 级聚合打分

把候选按 session 分组（已按 bm25 升序，故 `hits[0]` 即该 session 最强命中），
每个 session 算一个加权和。**bm25 是负数、越小越好**，先取 `rel = -bm25` 再
在候选集内 min-max 归一，让各分量可比、权重可解释。

分量：

| 分量 | 公式 | 含义 |
|---|---|---|
| `normBest` | `(rel₀ - relMin) / (relMax - relMin)` | 最强单点命中，归一到 [0,1]。**精度**。 |
| `cover` | `log1p(hitCount) / covMax` | 命中条数的对数（log 阻尼防长会话霸榜），归一。**aboutness / 广度**。 |
| `kindBonus` | `KIND_BONUS[hits₀.kind]` | 最强命中所在 kind 的权重。 |
| `recency` | `exp(-ln2 · ageDays / halfLife)` | 指数时间衰减，半衰期 `halfLife` 天时值为 0.5。**情景先验**。 |
| `titleHit` | `1 if 任一 query 词 ∈ title else 0` | 原生标题命中是强主题信号。无标题时为 0。 |

`KIND_BONUS`：`user 1.0 > assistant 0.8 > system 0.5 > thinking 0.4 >
tool_call 0.3`（其余 0.2）。依据 §1.3：命中在人类诉求原文里，比埋在工具参数里
更说明主题。

**session 总分**：

```
score = W.best·normBest + W.cover·cover + W.kind·kindBonus
      + W.recency·recency + W.title·titleHit
```

### 2.4 ④血缘 collapse（tree B）

`sess` 保留两棵树（见 `types.ts`）：Tree A 是会话内 event→event 链
（`parent_id`）；**Tree B 是会话间 fork/subagent**（`parent_session_id` +
`forked_from`）。检索去重针对 Tree B。

对每个候选 session，沿 `parent_session_id` → `forked_from` 原点向上走到
**lineage root**（`lineageRoot()`，带 cycle guard；`forked_from` 形如
`agent:sid#messageUuid`，只取 `agent:sid` 前缀；指向未知 session 则停在该点）：

```
lineageRoot(id):
  cur = id; seen = {id}
  loop:
    next = edges[cur].parent ?? edges[cur].forked
    if next 不存在 / 不在库 / 已 seen: return cur   # 到根了
    seen += next; cur = next
```

按 root 分组，组内**得分最高者为代表 `rep`**，其余进 `related[]`（标注
`fork`/`subagent`/`member` + 各自分数）。代表的**聚合分**再叠加组内其他成员的
阻尼贡献：

```
aggScore = rep.score + W.lineage · Σ(others.score)
```

效果：`一个父会话 + 3 个 subagent` 排成**一个强单元**，而不是四个互相稀释的弱
条目。`--no-collapse` 关闭（退回按 session_id）。

> **血缘边全部在入库阶段填好，不依赖 LLM**：OpenCode subagent（`session.parent_id`）、
> Claude fork（`forked_from`）、Claude subagent、以及 Codex subagent。三种 subagent
> 的父子关系派生方向不同：
>
> - **Claude subagent**：transcript 嵌在 `<enc-cwd>/<parent-uuid>/subagents/agent-<hash>.jsonl`，
>   父会话就是**目录名**，故 `parsers/claude.ts` 递归扫描后按路径派生身份
>   `claude:<parent-uuid>/agent-<hash>` 并直接填 `parent_session_id`（subagent 文件
>   自带的 `sessionId` 恰是父 uuid，会撞键，故不能用它）。角色 `agentType` 取自同
>   目录 `.meta.json` sidecar。**子侧可自证父**。
> - **Codex subagent**：方向相反——子会话是**普通顶层 rollout 文件**（`session_meta.id`
>   == 父在 `spawn_agent` 输出里拿到的 `agent_id`），**对父一无所知**。唯一的边存在
>   于**父** rollout 的 `spawn_agent` tool call：其 `function_call_output`（按 `call_id`
>   配对）返回 `{agent_id, nickname}`，`agent_id` 即子 session id。故 `parsers/codex.ts`
>   的 `buildLineage()` 做一次**跨文件预扫**，配对所有 spawn_agent call↔output 得
>   `childId → {parentId, agentType}`，`listSessions` 据此回填子会话的 `parent_session_id`
>   + 角色。`agentType` 取自 call 的 `arguments.agent_type`（explorer/worker/awaiter/
>   default）。**只有父侧记录这条边**，故必须全量预扫、不能按 `since` 裁剪（子在窗口
>   内、父的 spawn 记录可能在窗口外）。

### 2.5 ⑤上下文装配

对 top-N 代表会话，用**两条批量 SQL**（无 N+1）取回足够判断相关性的上下文：

1. **窗口查询**：主命中 `idx ± 5`（`MAIN_WINDOW`）；再挑 ≤2 个（`MAX_SECONDARY`）
   距主命中 >window、彼此也 >2×window 的**远距次命中簇** `idx ± 3`。用
   `WHERE (session_id=? AND idx BETWEEN ? AND ?) OR (...) ...` 一次拉齐。
   → "开头提问命中 + 结尾结论命中"两处都能看到，对判断"这次到底解决没解决 X"
   很关键。
2. **bookend 查询**：首个 `user` turn（`ask`，`MIN(idx) WHERE kind='user'`）+
   末个 `assistant` turn（`outcome`，`MAX(idx) WHERE kind='assistant'`），用
   `UNION ALL` 一次取齐所有代表的两端。

每个 snippet 都带 `idx`，可零成本 `sess show <id> --around <idx>` 跳转。文本经
`preview()`（redact 秘密 + 折叠空白 + 200 字符封顶）保持可扫读。

## 3. 复杂度与边界

- **时间**：主 FTS 查询 O(命中数)，被 5000 封顶；聚合/collapse/装配都在有界候选
  上，O(candidates)。血缘边一次性全量加载（几千行 tiny row，比逐候选走 DB 便宜）。
- **内存**：候选 event ≤5000 行、代表 ≤limit（默认 20）；上下文两条批量查询各
  ≤ limit×(11 + 2×7 + 2) 行。均有界。
- **不 N+1**：session 元数据、上下文窗口、bookend 各一条批量 SQL。
- **停用词防爆**：两级封顶 + bm25 升序 LIMIT，保证留下的是全局最强批。

## 4. 可调参数

全部走环境变量，**无需重建 DB**。默认值见 `cmdSearch` 顶部的 `W`：

| env | 默认 | 作用 |
|---|---|---|
| `TRACEDB_W_BEST` | 1.0 | normBest 权重（精度主力） |
| `TRACEDB_W_COVER` | 0.3 | coverage 权重（aboutness） |
| `TRACEDB_W_KIND` | 0.25 | kind bonus 权重 |
| `TRACEDB_W_RECENCY` | 0.4 | recency 权重 |
| `TRACEDB_W_TITLE` | 0.2 | title 命中权重 |
| `TRACEDB_W_LINEAGE` | 0.15 | 血缘成员阻尼叠加权重 |
| `TRACEDB_HALFLIFE_DAYS` | 30 | recency 半衰期（天） |

flags：`--half-life DAYS` · `--no-recency` · `--no-collapse` · `--limit N` ·
`--agent/--cwd/--since/--kind`。

> **退化不变式（可回归验证）**：把除 `BEST` 外的所有权重置 0 且 `--no-collapse`，
> 排序退回纯 bm25 序——每个分量都是可独立关闭的叠加项。

## 5. 效果

在真库上的定性观察：

- **多词召回**：`netlify deploy` / `会话 检索` 这类"词不相邻"的查询稳定命中——
  召回臂生效。
- **情景先验对**：查 `会话 检索`，**设计本功能的那次会话**被 recency + coverage
  + user-turn kind bonus 共同推到前列。
- **上下文自足**：每条结果直接给出 `ask`（诉求）/ `outcome`（结论）+ 命中前后
  ±5，多数情况下**不用再开 `show`** 就能确认是不是要找的会话。
- **中英文混排**：jieba 分词 + 两臂 planner 对中英文一致工作。

## 6. FTS 索引门控（不变式）

`tool_result`/`usage`（`UNINDEXED_KINDS`）**存储但不进索引**——前者多是文件
dump / stdout 噪声，后者是数字。三处必须一致地按 kind 门控，否则噪声会和真实
内容竞争：INSERT/DELETE/UPDATE 触发器的 `WHEN` 子句，以及 `rebuildFts()`。

`rebuildFts()` **不能**用 FTS5 `INSERT(events_fts) VALUES('rebuild')`——该命令
直接重扫 content 表、绕过触发器门控，会把 `UNINDEXED_KINDS` 灌进索引。正确写法
是 `'delete-all'` + 门控 `INSERT … SELECT … WHERE kind NOT IN (UNINDEXED)`，与
INSERT 触发器完全一致。自检：`SELECT ev.kind, COUNT(*) FROM events_fts JOIN
events ev ON ev.id=events_fts.rowid WHERE events_fts MATCH '"netlify"' GROUP BY
ev.kind` 若出现 `tool_result`/`usage`，说明索引被 `'rebuild'` 路径污染过，
重跑 `trace-db reindex` 即可。

## 7. 未来方向

- 原生 agent 提供标题时，`titleHit` 分量才发挥作用；血缘 collapse 已在入库阶段落地。
- **kind 原生加权**：现在 kind 权重是聚合后的 per-session 乘子。若要 bm25 内部
  按 kind 加权需把 `events_fts` 列化——但那会破坏 external-content 的
  `'rebuild'` 路径（`reindex` 和 tokenizer-swap 都依赖它），权衡后**不做**。
- **查询扩展 / 同义**：目前不做 query rewrite（除 planner 的 OR 拆分）；若需要
  可在 planner 里加。
