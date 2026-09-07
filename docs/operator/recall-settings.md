# Recall settings

**Nav: Recall.** `/dashboard/admin/recall-settings`

How much memory one turn is allowed to pull, and how far the search may walk
to find it. Leave a field empty to keep the built-in default, shown as the
placeholder and repeated in its own column. Changes go live on the next turn:
both the MCP surface and this dashboard's chat read them per turn, so no
restart is needed.

Which link to open and when to stop are decisions the navigator makes from its
prompt, not numbers on this page. These are the resources.

| Setting | Default |
|---|---|
| Flat slot — top-K hits | 10 |
| Entry fan — how deep to look for doors | 30 |
| Fresh slot — top-K captures | 3 |
| Project docs — funnel floor | 0.45 |
| Flat slot — relevance floor | 0 |
| Navigator — depth (hops) | 2 |
| Navigator — pages per hop | 3 |
| Navigator — prose budget (chars) | 8000 |
| Navigator — candidate window | 16 |
| Navigator — decision max tokens | 600 |
| Due-soon slot — top-K facts | 3 |
| Due-soon slot — horizon (hours) | 168 |
| WHO YOU ARE — budget (chars) | 900 |
| History with user — budget (chars) | 1400 |
| WHO IS SPEAKING — budget (chars) | 2500 |
| Recent window — entries per user | 32 |
| Recent window — TTL (hours) | 4 |
| Recent window — budget (chars) | 1200 |

Each row carries its own one-line explanation on the page; the two worth
knowing before you touch anything are the **navigator depth**, which is
clamped to a hard cap of 10 however high you set it, and the **recent
window**, which is deliberately short because it serves the live thread and
not history.

Related: which model serves the navigator is
[the model slots](model-slots.md); what one nightly cycle may change is
[REM settings](rem-settings.md).
