// The kernel inspector: what a REPL has defined, and every instance of the
// drt command the terminal is running, refreshed twice a second, with the
// swarm's own verbs as buttons.
export function inspector(cli) {
  return {
    vars: [],
    rows: [],
    note: '',
    init() {
      setInterval(() => this.refresh(), 500);
    },
    refresh() {
      const s = cli.session;
      this.vars = JSON.parse(s?.variables() ?? '[]');   // [] unless the command is drt repl
      if (!s?.hasSwarm()) {
        this.rows = [];
        this.note = 'nothing running';
        return;
      }
      this.note = `${s.alive()} alive`;
      this.rows = Array.from(s.ids(), (id) => ({
        id,
        parent: s.parent(id),
        resident: s.resident(id),
        instructions: JSON.parse(s.usage(id) ?? '{}').instructions ?? '-',
        caps: JSON.parse(s.caps(id) ?? '[]').map((g) => g.capability).join(', '),
      }));
    },
    act(verb, id) {
      cli.session?.[verb](id);   // example: omits showing a refusal (each verb throws one)
      this.refresh();
    },
  };
}
