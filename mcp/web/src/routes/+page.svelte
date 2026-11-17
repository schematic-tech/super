<script module>
  function focusPanel(node) {
    const previous = document.activeElement;
    const shell = document.querySelector(".app-shell");
    shell?.setAttribute("inert", "");
    node.focus();
    const trap = (event) => {
      if (event.key !== "Tab") return;
      const items = [
        ...node.querySelectorAll('button, a, input, summary, [tabindex="0"]'),
      ];
      const first = items[0],
        last = items.at(-1);
      if (
        event.shiftKey &&
        (document.activeElement === first || document.activeElement === node)
      ) {
        event.preventDefault();
        last?.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first?.focus();
      }
    };
    node.addEventListener("keydown", trap);
    return {
      destroy() {
        shell?.removeAttribute("inert");
        node.removeEventListener("keydown", trap);
        previous?.focus();
      },
    };
  }
</script>

<script>
  import { onMount } from "svelte";
  import { checksFrom, category, status, revision } from "$lib/history.js";
  let checks = $state([]),
    directory = $state(""),
    updated = $state(""),
    error = $state("");
  let selectedNumber = $state(""),
    query = $state(""),
    filter = $state("all");
  let loading = $state(true),
    paging = $state(false),
    paused = $state(false),
    next = $state(null);
  let workspaceId,
    paginated = false;
  let token = "",
    controller,
    timer,
    refreshRunning = false,
    disposed = false;
  let selected = $derived(
    checks.find((check) => String(check.number) === selectedNumber),
  );
  let visible = $derived(
    checks.filter(
      (check) =>
        (filter === "all" ||
          (filter === "attention"
            ? ["fail", "conditional", "error"].includes(category(check))
            : category(check) === filter)) &&
        `${check.supertest?.path} ${check.supertest?.name} ${check.number} ${status(check)}`
          .toLowerCase()
          .includes(query.toLowerCase()),
    ),
  );
  let counts = $derived({
    pass: checks.filter((c) => category(c) === "pass").length,
    attention: checks.filter((c) =>
      ["fail", "conditional", "error"].includes(category(c)),
    ).length,
    running: checks.filter((c) => category(c) === "running").length,
  });
  const when = (value) =>
    value
      ? new Date(value).toLocaleString([], {
          month: "short",
          day: "numeric",
          hour: "2-digit",
          minute: "2-digit",
        })
      : "—";
  const name = (check) => check.supertest?.name ?? `Check ${check.number}`;
  function merge(incoming) {
    const all = new Map(checks.map((check) => [String(check.number), check]));
    for (const check of incoming) {
      const previous = all.get(String(check.number));
      if (
        previous &&
        BigInt(previous.event_sequence ?? 0) > BigInt(check.event_sequence ?? 0)
      )
        continue;
      all.set(String(check.number), check);
    }
    checks = [...all.values()]
      .sort((a, b) => (BigInt(a.number) > BigInt(b.number) ? -1 : 1))
      .slice(0, 500);
  }
  async function request(route, signal) {
    const response = await fetch(route, {
      headers: { Authorization: `Bearer ${token}` },
      cache: "no-store",
      signal,
    });
    const data = await response.json();
    if (!response.ok)
      throw new Error(data.error ?? "Could not refresh history.");
    return data;
  }
  async function refresh() {
    if (refreshRunning || disposed || !token) return;
    refreshRunning = true;
    controller = new AbortController();
    try {
      const report = await request("/api/history", controller.signal);
      if (disposed) return;
      if (workspaceId !== undefined && workspaceId !== report.workspace_id) {
        checks = [];
        selectedNumber = "";
        paginated = false;
      }
      workspaceId = report.workspace_id;
      merge(checksFrom(report));
      directory = report.directory;
      updated = report.updated_at;
      error = "";
      if (!paginated) next = report.next_before;
      if (
        selectedNumber &&
        !checksFrom(report).some((c) => String(c.number) === selectedNumber)
      ) {
        const detail = await request(
          `/api/check/${selectedNumber}`,
          controller.signal,
        );
        if (!disposed) merge(checksFrom(detail));
      }
    } catch (failure) {
      if (!disposed && failure.name !== "AbortError") error = failure.message;
    } finally {
      refreshRunning = false;
      loading = false;
    }
  }
  async function older() {
    if (!next || paging || checks.length >= 500) return;
    paging = true;
    try {
      const report = await request(
        `/api/history?before=${encodeURIComponent(next)}`,
      );
      if (!disposed) {
        merge(checksFrom(report));
        next = report.next_before;
        paginated = true;
      }
    } catch (failure) {
      error = failure.message;
    } finally {
      paging = false;
    }
  }
  onMount(() => {
    token =
      new URLSearchParams(location.hash.slice(1)).get("token") ??
      sessionStorage.getItem("super-dashboard-token") ??
      "";
    if (token) {
      sessionStorage.setItem("super-dashboard-token", token);
      history.replaceState(null, "", location.pathname);
      refresh();
    } else {
      error = "Open the dashboard link from Super to connect to this worktree.";
      loading = false;
    }
    timer = setInterval(() => {
      if (!paused && !document.hidden) refresh();
    }, 2500);
    const visibleAgain = () => {
      if (!document.hidden && !paused) refresh();
    };
    document.addEventListener("visibilitychange", visibleAgain);
    return () => {
      disposed = true;
      clearInterval(timer);
      controller?.abort();
      document.removeEventListener("visibilitychange", visibleAgain);
    };
  });
</script>

<svelte:head
  ><title>Super history · Schematic</title><meta
    name="description"
    content="Live supertest history for your local worktree."
  /></svelte:head
>

<div class="app-shell">
  <aside class="sidebar">
    <a
      class="brand"
      href="https://schematic.tech"
      target="_blank"
      rel="noreferrer"
      aria-label="Schematic website"
      ><svg
        width="28"
        height="28"
        viewBox="0 0 28 28"
        fill="none"
        aria-hidden="true"
        ><path
          d="M5 7h12l6 7-6 7H5l6-7-6-7Z"
          stroke="currentColor"
          stroke-width="2"
        /><path d="M11 7l6 7-6 7" stroke="currentColor" stroke-width="2" /></svg
      ><span>schematic</span></a
    >
    <div class="workspace-label">WORKSPACE</div>
    <div class="workspace">
      <span class="product-mark">S</span>
      <div><strong>Super</strong><span>Local worktree</span></div>
    </div>
    <nav aria-label="Dashboard">
      <a
        class="nav-active"
        href="/"
        onclick={(event) => {
          event.preventDefault();
          selectedNumber = "";
        }}
        ><span aria-hidden="true">◷</span> Supertest history
        <span class="nav-count">{checks.length || "—"}</span></a
      >
    </nav>
    <div class="sidebar-bottom">
      <span class="local-dot"></span> Runs on your machine
      <a
        href="https://docs.schematic.tech/super/"
        target="_blank"
        rel="noreferrer"
        >Super documentation <span aria-hidden="true">↗</span></a
      >
    </div>
  </aside>
  <main>
    <header class="topbar">
      <span
        >Schematic <span class="breadcrumb">/</span> Super
        <span class="breadcrumb">/</span> <strong>History</strong></span
      ><span class="connection" class:offline={!!error || paused}
        ><span></span>{error
          ? "Reconnecting"
          : paused
            ? "Paused"
            : "Live"}</span
      >
    </header>
    <div class="page">
      <div class="page-heading">
        <div>
          <div class="eyebrow">SUPER / CHECKS</div>
          <h1>Supertest history<span class="heading-dot">.</span></h1>
          <p class="worktree" title={directory}>
            {directory || "Connecting to your worktree…"}
          </p>
        </div>
        <div class="actions">
          <button
            class="button"
            onclick={() => {
              paused = !paused;
              if (!paused) refresh();
            }}>{paused ? "Resume live" : "Pause live"}</button
          ><button class="button primary" aria-label="Refresh" onclick={refresh}
            >↻ <span>Refresh</span></button
          >
        </div>
      </div>
      <section class="metrics" aria-label="Recent check summary">
        <div class="metric">
          <span>Recent checks</span><strong
            >{checks.length || (loading ? "—" : "0")}</strong
          ><small>Loaded history</small>
        </div>
        <div class="metric">
          <span><i class="dot pass"></i> Passed</span><strong
            >{counts.pass}</strong
          ><small>Reported passing outcomes</small>
        </div>
        <div class="metric">
          <span><i class="dot attention"></i> Needs attention</span><strong
            >{counts.attention}</strong
          ><small>Findings and operational errors</small>
        </div>
        <div class="metric">
          <span><i class="dot running"></i> In progress</span><strong
            >{counts.running}</strong
          ><small>Awaiting a result</small>
        </div>
      </section>
      {#if error}<div class="notice" role="status">
          <strong>History is not up to date.</strong>
          {error}
          {#if checks.length}Your last received results are still shown.{/if}
        </div>{/if}
      <section class="history-card" aria-label="Supertest history">
        <div class="list-toolbar">
          <div class="filters" aria-label="Filter checks">
            {#each [["all", "All checks"], ["attention", "Needs attention"], ["running", "In progress"], ["pass", "Passed"]] as [value, label]}<button
                class:active={filter === value}
                aria-pressed={filter === value}
                onclick={() => (filter = value)}>{label}</button
              >{/each}
          </div>
          <label class="search"
            ><span aria-hidden="true">⌕</span><input
              type="search"
              bind:value={query}
              placeholder="Find a supertest…"
              aria-label="Search supertests"
            /></label
          >
        </div>
        <div class="table-scroll">
          <table>
            <thead
              ><tr
                ><th>Supertest</th><th>Result</th><th>Source</th><th>Updated</th
                ><th><span class="sr-only">Details</span></th></tr
              ></thead
            ><tbody>
              {#each visible as check (String(check.number))}<tr
                  class:selected={selectedNumber === String(check.number)}
                  ><td
                    ><button
                      class="check-name"
                      onclick={() => (selectedNumber = String(check.number))}
                      >{name(check)}</button
                    >
                    <div class="check-path">
                      {check.supertest?.path ?? "Supertest"}
                      <span>· #{check.number}</span>
                    </div></td
                  ><td
                    ><span class="status {category(check)}"
                      ><i></i>{status(check)}</span
                    >{#if check.updates_pending}<div class="pending">
                        Updates pending
                      </div>{/if}</td
                  ><td
                    ><code class="revision" title={revision(check)}
                      >{revision(check).slice(0, 7) || "—"}</code
                    ></td
                  ><td class="timestamp">{when(check.updated_at)}</td><td
                    ><button
                      class="detail-button"
                      aria-label={`View check ${check.number}`}
                      onclick={() => (selectedNumber = String(check.number))}
                      >↗</button
                    ></td
                  ></tr
                >{/each}
            </tbody>
          </table>
        </div>
        {#if !visible.length}<div class="empty">
            <span aria-hidden="true">{loading ? "◌" : "◇"}</span>
            <h2>
              {loading
                ? "Loading your checks"
                : checks.length
                  ? "No matching supertests"
                  : "Your checks will appear here"}
            </h2>
            <p>
              {loading
                ? "Reading the latest history from Super."
                : checks.length
                  ? "Try another name, path, or result filter."
                  : "Run a Super check in this worktree to see its results and history."}
            </p>
          </div>{/if}
        <footer class="list-footer">
          <span
            >{visible.length}
            {visible.length === 1 ? "check" : "checks"}{updated
              ? ` · Refreshed ${new Date(updated).toLocaleTimeString()}`
              : ""}</span
          >{#if next && checks.length < 500}<button
              class="button small"
              disabled={paging}
              onclick={older}
              >{paging ? "Loading…" : "Load older checks ↓"}</button
            >{:else if checks.length >= 500}<span
              >Showing the 500 most recent loaded checks</span
            >{:else}<span>End of loaded history</span>{/if}
        </footer>
      </section>
      <footer class="page-footer">
        <span>Super by Schematic</span><span
          >Read-only history · No checks are started here</span
        >
      </footer>
    </div>
  </main>
</div>

{#if selected}
  <div
    class="detail-overlay"
    role="presentation"
    onclick={(event) => {
      if (event.target === event.currentTarget) selectedNumber = "";
    }}
  >
    <div
      class="detail-panel"
      role="dialog"
      aria-modal="true"
      aria-labelledby="detail-title"
      tabindex="-1"
      use:focusPanel
      onkeydown={(event) => {
        if (event.key === "Escape") selectedNumber = "";
      }}
    >
      <div class="detail-top">
        <span class="eyebrow">CHECK #{selected.number}</span><button
          class="close-button"
          onclick={() => (selectedNumber = "")}
          aria-label="Close check details">×</button
        >
      </div>
      <span class="status {category(selected)}"><i></i>{status(selected)}</span>
      <h2 id="detail-title">{name(selected)}</h2>
      <p class="detail-path">{selected.supertest?.path}</p>
      <dl class="detail-meta">
        <div>
          <dt>Source</dt>
          <dd>
            <code>{revision(selected).slice(0, 12) || "Unavailable"}</code>
          </dd>
        </div>
        <div>
          <dt>Updated</dt>
          <dd>{when(selected.updated_at)}</dd>
        </div>
        <div>
          <dt>Outcome</dt>
          <dd>{selected.result?.outcome ?? "No result"}</dd>
        </div>
        <div>
          <dt>Assurance</dt>
          <dd>{selected.result?.assurance || "Not reported"}</dd>
        </div>
      </dl>
      {#if selected.updates_pending}<div class="live-note">
          <i class="dot running"></i> Further updates are pending.
        </div>{/if}
      {#if selected.operational_error}<div class="notice">
          <strong>Operational error</strong>
          <p>
            {selected.operational_error.message ??
              JSON.stringify(selected.operational_error)}
          </p>
        </div>{/if}
      {#if selected.presentation?.live_line}<p class="live-note">
          {selected.presentation.live_line}
        </p>{/if}
      <div class="detail-section">
        <h3>Evidence & explanation</h3>
        {#if selected.presentation?.details?.length}{#each selected.presentation.details as line}<p
              class:emphasized={line.emphasized}
            >
              {line.text}
            </p>{/each}{:else}<p class="muted">
            No explanation has been received yet.
          </p>{/if}
      </div>
      {#if selected.fix}<div class="detail-section">
          <h3>Proposed fix</h3>
          <p>{selected.fix.summary}</p>
          <pre class="patch"><code>{selected.fix.diff}</code></pre>
          {#if selected.fix.instructions}<p>
              {selected.fix.instructions}
            </p>{/if}{#if selected.fix.validation?.length}<h4>
              Suggested validation
            </h4>
            {#each selected.fix.validation as command}<pre><code>{command}</code
                ></pre>{/each}{/if}
          <p class="muted">
            Review and apply this proposal through Super or your agent.
          </p>
        </div>{:else if selected.fix_pending}<p class="live-note">
          A fix proposal may still arrive.
        </p>{/if}
      <details class="raw-result">
        <summary>Exact check data</summary>
        <pre>{JSON.stringify(selected, null, 2)}</pre>
      </details>
    </div>
  </div>
{/if}
