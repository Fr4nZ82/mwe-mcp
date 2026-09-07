# REM settings

**Nav: REM.** `/dashboard/admin/rem-settings`

What the night is allowed to do, and how much of it per cycle. Leave a field
empty to keep the built-in default, which the page shows as the placeholder
and repeats in its own column. Changes go live on the next cycle: saving
rewrites `mwe-mcp.config.yaml` atomically and hot-swaps the running policy, so
no restart is needed.

Semantic judgment — *what* to merge, promote or rewrite — is not here. It lives
in the passes themselves. These are the ceilings.

| Setting | Default | What it bounds |
|---|---|---|
| Split a prose page — facts it must hold first | 8 | The size floor below which the split pass does not even look at a page. |
| Split a technical-prose page — facts it must hold first | 32 | The same floor for a page read point by point. A list page is never split on size. |
| Found a new wiki — pages on one subject it takes | 9 | Pages on one subject needed before a new wiki is founded. Birth only. |
| Splitting and founding — changes per cycle | 5 | Changes to the shape of the memory per cycle; splitting and founding share it. |
| Merging two pages — pairs checked per cycle | 3 | Pairs sent to the model to confirm. `0` turns the pass off. |
| Moving a page to another wiki — moves per cycle | 3 | The only pass that can move a page out of the wiki it was born in. |
| Closing what has been overtaken — facts checked per cycle | 8 | Facts that look like evidence something older is finished. |
| Facts that contradict each other — starting points per cycle | 8 | Where the contradiction sweep starts from. |
| Putting dates in order — facts per cycle | 16 | Facts whose wording looks like a date ("last Tuesday"), oldest first. |
| Repairing where a fact came from — repairs per cycle | 32 | No model call; this one only re-reads. |
| A new comment is left alone for — seconds | 900 | How long a fresh comment sits untouched before the cycle reads it. |
| Emptied pages — files removed per cycle | 4 | Page files with nothing live left on them. |
| Something the memory failed to find — cases judged per cycle | 3 | Recorded recall misses judged per cycle. |
| Same fact missed this many times — then you are told | 3 | Misses before a notice is raised. Nothing is ever changed on its own from it. |

Every `0` in that table turns its pass off, where the page says so.

Related: when the runs fire is the dream cadence on [Settings](settings.md);
what each pass costs is [Usage](usage-and-spend.md).
