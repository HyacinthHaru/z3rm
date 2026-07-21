// z3rm-tab-bar — tab strip chrome (spec §5.5, event-driven).
//
// VDOM props cross the JS→Rust FFI as JSON, so click handlers are serialized
// command descriptors ({ command, args }), never closures; the native bridge
// dispatches them through the command registry. Routing clicks as commands
// also makes tab actions visible to the command palette.

export function activate(context) {
    const state = {
        // [{ id, title, paneId, active }]
        tabs: [],
    };

    const TabBarView = {
        render() {
            return {
                type: 'div',
                props: { id: 'tab-bar' },
                style: { flexDirection: 'row', gap: '2px' },
                children: state.tabs.map((tab) => ({
                    type: 'button',
                    props: {
                        id: `tab-${tab.id}`,
                        class: tab.active ? 'tab active' : 'tab',
                        title: tab.title,
                        onClick: { command: 'z3rm.tab-bar.focus-tab', args: [tab.id] },
                    },
                    children: [tab.title],
                })),
            };
        },
    };

    // tab:title payloads carry { tabId, title, paneId, active }; title events
    // double as the tab lifecycle signal, so entries are upserted here.
    context.mux.subscribe('tab:title', (tab) => {
        const existing = state.tabs.find((t) => t.id === tab.tabId);
        if (existing) {
            existing.title = tab.title;
            existing.paneId = tab.paneId || existing.paneId;
        } else {
            state.tabs.push({
                id: tab.tabId,
                title: tab.title,
                paneId: tab.paneId,
                active: false,
            });
        }
        if (tab.active) {
            state.tabs.forEach((t) => {
                t.active = t.id === tab.tabId;
            });
        }
        TabBarView.invalidate();
    });

    context.commands.register('z3rm.tab-bar.focus-tab', (tabId) => {
        const tab = state.tabs.find((t) => t.id === tabId);
        // Focusing a tab's last-active pane is how the mux switches tabs.
        if (tab && tab.paneId) {
            context.mux.focusPane(tab.paneId);
        }
    });

    context.registerChromeView('tab-bar', TabBarView);
}
