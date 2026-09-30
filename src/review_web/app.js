const apiRoot = window.location.pathname.replace(/\/$/, "");

const ui = {
  file: document.querySelector("#file"),
  progress: document.querySelector("#progress"),
  finish: document.querySelector("#finish"),
  search: document.querySelector("#search"),
  reviewer: document.querySelector("#reviewer"),
  selectVisible: document.querySelector("#select-visible"),
  approveSelected: document.querySelector("#approve-selected"),
  approveAll: document.querySelector("#approve-all"),
  list: document.querySelector("#entry-list"),
  detail: document.querySelector("#detail"),
  toast: document.querySelector("#toast"),
  filters: [...document.querySelectorAll("[data-filter]")],
};

const view = {
  data: null,
  filter: "pending",
  activeKey: null,
  selectedKeys: new Set(),
};

function element(tag, className, text) {
  const output = document.createElement(tag);
  if (className) output.className = className;
  if (text !== undefined) output.textContent = text;
  return output;
}

async function api(path, options = {}) {
  const response = await fetch(`${apiRoot}${path}`, options);
  if (!response.ok) throw new Error(await response.text());
  return response.json();
}

function post(path, payload) {
  return api(path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: payload === undefined ? undefined : JSON.stringify(payload),
  });
}

function visibleEntries() {
  const query = ui.search.value.trim().toLowerCase();
  return view.data.entries.filter((entry) => {
    const matchesFilter =
      view.filter === "all" ||
      (view.filter === "pending" && (entry.status !== "verified" || pendingProposals(entry.id).length)) ||
      entry.status === view.filter;
    const searchable = [entry.id, entry.title, entry.authors, entry.year]
      .filter(Boolean)
      .join(" ")
      .toLowerCase();
    return matchesFilter && (!query || searchable.includes(query));
  });
}

function pendingProposals(key) {
  return view.data.proposals.filter((proposal) => proposal.target === key && !proposal.decision);
}

function render() {
  ui.file.textContent = view.data.file;
  const proposals = view.data.proposals.filter((proposal) => !proposal.decision).length;
  ui.progress.textContent = `${view.data.verified} / ${view.data.total} verified${proposals ? ` · ${proposals} proposal(s) pending` : ""}`;
  renderList();
  renderDetail();
  renderSelectionControls();
}

function renderList() {
  const entries = visibleEntries();
  ui.list.replaceChildren();
  if (!entries.length) ui.list.append(element("p", "empty", "No entries in this view."));

  for (const entry of entries) {
    const row = element("div", `entry-row${entry.id === view.activeKey ? " selected" : ""}`);
    const checkbox = document.createElement("input");
    checkbox.type = "checkbox";
    checkbox.checked = view.selectedKeys.has(entry.id);
    checkbox.disabled = entry.status === "verified";
    checkbox.addEventListener("click", (event) => {
      event.stopPropagation();
      checkbox.checked ? view.selectedKeys.add(entry.id) : view.selectedKeys.delete(entry.id);
      renderSelectionControls();
    });

    const summary = element("div");
    summary.append(element("div", "entry-title", entry.title || entry.id));
    if (pendingProposals(entry.id).length) summary.append(element("div", "meta", `${pendingProposals(entry.id).length} agent proposal(s)`));
    summary.append(
      element("div", "meta", [entry.id, entry.authors, entry.year].filter(Boolean).join(" · ")),
    );
    row.append(checkbox, summary, element("span", `status ${entry.status}`, entry.status));
    row.addEventListener("click", () => {
      view.activeKey = entry.id;
      render();
    });
    ui.list.append(row);
  }

  if (!view.activeKey || !view.data.entries.some((entry) => entry.id === view.activeKey)) {
    view.activeKey = entries[0]?.id || null;
  }
}

function renderDetail() {
  ui.detail.replaceChildren();
  const entry = view.data.entries.find((candidate) => candidate.id === view.activeKey);
  if (!entry) {
    ui.detail.append(element("p", "empty", "Select an entry to inspect its evidence."));
    return;
  }

  ui.detail.append(element("h1", "", entry.title || "Untitled entry"));
  ui.detail.append(element("div", "citation-key", entry.id));
  const humanApproved = entry.approval && entry.approval.contentHash === entry.contentHash;
  const evidence = entry.approval
    ? humanApproved ? "Valid human approval" : "Approval no longer matches current content"
    : entry.source.valid ? "Valid" : `Invalid: ${entry.source.error || "unknown error"}`;
  for (const [label, value] of [
    ["Status", entry.status],
    ["Entry type", entry.entryType],
    ["Source kind", humanApproved ? "human" : entry.source.kind],
    ["Source key", humanApproved ? entry.approval.id : entry.source.key],
    ["Evidence", evidence],
  ]) {
    ui.detail.append(summaryField(label, value || "—"));
  }
  if (entry.approval) ui.detail.append(summaryField("Reviewer", entry.approval.reviewer || "Anonymous"));

  const fields = element("section", "all-fields");
  fields.append(element("h2", "", "Current BibTeX fields"));
  fields.append(singleFieldTable(entry.fields));
  ui.detail.append(fields);

  const proposals = view.data.proposals.filter((proposal) => proposal.target === entry.id)
    .sort((left, right) => Number(Boolean(left.decision)) - Number(Boolean(right.decision)));
  for (const proposal of proposals) {
    if (proposal.decision) {
      const history = element("details", "agent-proposal-history");
      history.append(element("summary", "", `Agent proposal: ${proposal.decision.outcome} · ${proposal.agent}`));
      history.append(proposalView(proposal));
      ui.detail.append(history);
    } else ui.detail.append(proposalView(proposal));
  }

  if (entry.status !== "verified") ui.detail.append(providerSearch(entry));
  ui.detail.append(bibtexEditor(entry));
  ui.detail.append(detailActions(entry));
}

function proposalView(proposal) {
  const section = element("section", "agent-proposal");
  section.append(element("h2", "", "Agent proposal"));
  section.append(summaryField("Agent", proposal.agent));
  section.append(summaryField("Reason", proposal.reason));
  if (proposal.provider) {
    section.append(summaryField("Provider", `${proposal.provider.record.provider}: ${proposal.provider.record.id}`));
    section.append(summaryField("Title / author match", proposal.matchScore ? `${proposal.matchScore.score} (title ${proposal.matchScore.title_score}, author ${proposal.matchScore.author_score}; threshold ${proposal.provider.threshold})` : "Cannot be scored"));
    section.append(summaryField("Agent adoption", proposal.agentAdoptable ? "Eligible after agent review" : (proposal.agentAdoptionBlocker || "Already decided")));
  } else section.append(summaryField("Provider support", "None; human approval required"));
  if (proposal.decision?.agent) section.append(summaryField("Adopted by agent", proposal.decision.agent));
  section.append(summaryField("Created", new Date(proposal.timestamp * 1000).toLocaleString()));
  section.append(summaryField("Decision", proposal.decision?.outcome || (proposal.stale ? "Stale baseline" : "Pending")));
  const before = { ENTRYTYPE: proposal.before.entry_type, ...proposal.before.fields };
  const after = { ENTRYTYPE: proposal.after.entry_type, ...proposal.after.fields };
  const rows = [...new Set([...Object.keys(before), ...Object.keys(after)])].sort().map((field) => ({
    field,
    current: before[field] ?? null,
    proposed: after[field] ?? null,
    state: before[field] === after[field] ? "unchanged" : !(field in before) ? "added" : !(field in after) ? "removed" : "changed",
  }));
  section.append(comparisonTable(rows, "Before proposal", "Agent replacement"));
  if (proposal.provider || proposal.evidence !== null) {
    const details = element("details");
    details.append(element("summary", "", "Supporting evidence"));
    details.append(element("pre", "proposal-evidence", JSON.stringify(proposal.provider || proposal.evidence, null, 2)));
    section.append(details);
  }
  if (!proposal.decision) {
    const actions = element("div", "editor-actions");
    const adopt = element("button", "primary", "Adopt proposal and approve");
    adopt.disabled = proposal.stale;
    adopt.addEventListener("click", () => decideProposal(proposal.id, true));
    const reject = element("button", "", "Reject proposal");
    reject.addEventListener("click", () => decideProposal(proposal.id, false));
    actions.append(adopt, reject);
    section.append(actions);
  }
  return section;
}

async function decideProposal(id, adopt) {
  try {
    view.data = await post("/api/proposal", { id, adopt, reviewer: ui.reviewer.value });
    notify(adopt ? "Proposal adopted and human-verified" : "Proposal rejected");
    render();
  } catch (error) { notify(error.message, true); }
}

function summaryField(label, value) {
  const row = element("div", "summary-field");
  row.append(element("div", "label", label), element("div", "value", value));
  return row;
}

function singleFieldTable(fields) {
  const rows = Object.entries(fields).map(([field, value]) => ({
    field,
    current: value,
    proposed: null,
    state: "unchanged",
  }));
  return comparisonTable(rows, "Value", null);
}

function comparisonTable(rows, currentLabel, proposedLabel) {
  const wrap = element("div", "comparison-wrap");
  const table = element("div", `comparison${proposedLabel === null ? " two-column" : ""}`);
  const header = element("div", "comparison-row comparison-head");
  header.append(element("div", "", "Field"), element("div", "", currentLabel));
  if (proposedLabel !== null) header.append(element("div", "", proposedLabel));
  table.append(header);

  for (const field of rows) {
    const row = element("div", `comparison-row ${field.state}`);
    row.append(element("div", "field-name", field.field));
    row.append(element("div", "", field.current ?? "—"));
    if (proposedLabel !== null) row.append(element("div", "", field.proposed ?? "—"));
    table.append(row);
  }
  wrap.append(table);
  return wrap;
}

function providerSearch(entry) {
  const section = element("section", "matches");
  section.append(element("h2", "", "Provider title matches"));
  const controls = element("div", "provider-actions");
  const results = element("div");
  for (const provider of [
    { id: "crossref", label: "Crossref" },
    { id: "openreview", label: "OpenReview" },
  ]) {
    const button = element("button", provider.id === "crossref" ? "primary" : "", `Search ${provider.label}`);
    button.addEventListener("click", () => searchProvider(entry, provider, results));
    controls.append(button);
  }
  section.append(controls, results);
  return section;
}

function bibtexEditor(entry) {
  const section = element("section", "bibtex-editor");
  section.append(element("h2", "", "Paste BibTeX"));
  const textarea = document.createElement("textarea");
  textarea.rows = 9;
  textarea.spellcheck = false;
  textarea.placeholder = "@article{any-key,\n  title = {...},\n  author = {...}\n}";
  const controls = element("div", "editor-actions");
  const previewButton = element("button", "", "Preview pasted entry");
  const preview = element("div");
  textarea.addEventListener("input", () => preview.replaceChildren());
  previewButton.addEventListener("click", async () => {
    preview.replaceChildren(element("p", "meta", "Parsing BibTeX…"));
    try {
      const bibtex = textarea.value;
      const result = await post("/api/bibtex/preview", { key: entry.id, bibtex });
      if (view.activeKey !== entry.id || textarea.value !== bibtex) return;
      preview.replaceChildren();
      const keyMessage = result.pastedKey === entry.id
        ? `Citation key: ${entry.id}`
        : `Pasted key ${result.pastedKey} will be kept as ${entry.id}`;
      preview.append(element("p", "meta", `@${result.entryType} · ${keyMessage}`));
      preview.append(comparisonTable(result.comparison, "Current", "Pasted BibTeX"));
      const apply = element("button", "primary", "Use pasted BibTeX and approve");
      apply.addEventListener("click", () => applyPastedBibtex(entry.id, bibtex, result));
      preview.append(apply);
    } catch (error) {
      preview.replaceChildren(element("p", "form-error", error.message));
    }
  });
  controls.append(previewButton);
  section.append(textarea, controls, preview);
  return section;
}

async function searchProvider(entry, provider, results) {
  results.replaceChildren(element("p", "meta", `Searching ${provider.label}…`));
  try {
    const response = await post("/api/candidates", { key: entry.id, provider: provider.id });
    if (view.activeKey !== entry.id) return;
    renderCandidates(entry, provider, response, results);
  } catch (error) {
    results.replaceChildren(element("p", "meta", error.message));
    const retry = element("button", "", "Retry");
    retry.addEventListener("click", () => searchProvider(entry, provider, results));
    results.append(retry);
  }
}

function renderCandidates(entry, provider, response, results) {
  results.replaceChildren(
    element(
      "p",
      "meta",
      `${provider.label} title query: ${response.query} · agent threshold ${response.threshold.toFixed(2)}`,
    ),
  );
  const candidates = [...response.candidates].sort(
    (left, right) => (right.similarity?.score || 0) - (left.similarity?.score || 0),
  );
  if (!candidates.length) {
    results.append(element("p", "empty", `${provider.label} returned no candidates.`));
    return;
  }
  for (const candidate of candidates) results.append(candidateView(entry, provider, candidate));
}

function candidateView(entry, provider, candidate) {
  const card = element("article", "candidate");
  const heading = element("div", "candidate-head");
  const identity = element("div");
  identity.append(element("div", "candidate-title", candidate.record.title || candidate.record.id));
  identity.append(element("div", "meta", `@${candidate.entryType} · ${candidate.record.id}`));
  heading.append(identity);
  if (candidate.adoptable) heading.append(element("div", "adoptable", "Agent-adoptable"));
  card.append(heading);

  const score = candidate.similarity;
  card.append(
    element(
      "div",
      "scores",
      score
        ? `Match ${score.score.toFixed(4)} · title ${score.title_score.toFixed(4)} · author ${score.author_score.toFixed(4)}`
        : "Cannot score: missing title or author",
    ),
  );
  card.append(comparisonTable(candidate.comparison, "Current", provider.label));
  const adopt = element("button", "primary", `Adopt ${provider.label} record`);
  adopt.addEventListener("click", () => adoptCandidate(entry.id, candidate.record.id, provider, candidate.previewHash));
  card.append(adopt);
  return card;
}

function detailActions(entry) {
  const actions = element("div", "detail-actions");
  if (entry.url) {
    const open = element("button", "", "Open source");
    open.addEventListener("click", () => window.open(entry.url, "_blank", "noopener"));
    actions.append(open);
  }
  const approve = element("button", "", "Approve current entry");
  approve.disabled = entry.status === "verified" || pendingProposals(entry.id).length > 0;
  approve.addEventListener("click", () => approveKeys([entry.id]));
  actions.append(approve);
  return actions;
}

function renderSelectionControls() {
  ui.approveSelected.disabled = view.selectedKeys.size === 0;
  ui.approveSelected.textContent = view.selectedKeys.size
    ? `Approve selected (${view.selectedKeys.size})`
    : "Approve selected";
  ui.approveAll.disabled = view.data.verified === view.data.total;
}

async function approveKeys(keys) {
  try {
    const hashes = Object.fromEntries(keys.map((key) => [key, view.data.entries.find((entry) => entry.id === key).contentHash]));
    view.data = await post("/api/approve", { keys, hashes, reviewer: ui.reviewer.value });
    keys.forEach((key) => view.selectedKeys.delete(key));
    notify(`Approved ${keys.length} ${keys.length === 1 ? "entry" : "entries"}`);
    render();
  } catch (error) {
    notify(error.message, true);
  }
}

async function adoptCandidate(key, id, provider, previewHash) {
  try {
    view.data = await post("/api/adopt", {
      key,
      id,
      provider: provider.id,
      reviewer: ui.reviewer.value,
      preview_hash: previewHash,
    });
    view.selectedKeys.delete(key);
    notify(`${provider.label} record adopted and provider-verified`);
    render();
  } catch (error) {
    notify(error.message, true);
  }
}

async function applyPastedBibtex(key, bibtex, preview) {
  try {
    view.data = await post("/api/bibtex/apply", {
      key,
      bibtex,
      reviewer: ui.reviewer.value,
      baseline_hash: preview.baselineHash,
      proposal_hash: preview.proposalHash,
    });
    view.selectedKeys.delete(key);
    notify("Pasted BibTeX saved and human-verified");
    render();
  } catch (error) {
    notify(error.message, true);
  }
}

function notify(message, isError = false) {
  ui.toast.textContent = message;
  ui.toast.classList.toggle("error", isError);
  ui.toast.classList.add("show");
  window.setTimeout(() => ui.toast.classList.remove("show"), 3500);
}

function bindEvents() {
  for (const button of ui.filters) {
    button.addEventListener("click", () => {
      view.filter = button.dataset.filter;
      ui.filters.forEach((candidate) => candidate.classList.toggle("active", candidate === button));
      render();
    });
  }
  ui.search.addEventListener("input", render);
  ui.selectVisible.addEventListener("click", () => {
    visibleEntries()
      .filter((entry) => entry.status !== "verified")
      .forEach((entry) => view.selectedKeys.add(entry.id));
    render();
  });
  ui.approveSelected.addEventListener("click", () => approveKeys([...view.selectedKeys]));
  ui.approveAll.addEventListener("click", () => {
    const keys = view.data.entries.filter((entry) => entry.status !== "verified" && !pendingProposals(entry.id).length).map((entry) => entry.id);
    const reviewer = ui.reviewer.value.trim() || "Anonymous";
    if (window.confirm(`Approve all ${keys.length} remaining entries as ${reviewer}?`)) approveKeys(keys);
  });
  ui.finish.addEventListener("click", async () => {
    await post("/api/finish");
    document.body.replaceChildren(element("p", "empty", "Review finished. You can close this tab."));
  });
}

async function start() {
  bindEvents();
  try {
    view.data = await api("/api/state");
    ui.reviewer.value = view.data.reviewerDefault;
    view.activeKey =
    view.data.entries.find((entry) => pendingProposals(entry.id).length)?.id ||
    view.data.entries.find((entry) => entry.status !== "verified")?.id || view.data.entries[0]?.id;
    render();
  } catch (error) {
    notify(error.message, true);
  }
}

start();
