// z3rm-status-bar — bottom status line (spec §5.5).
//
// Mixed update strategy per spec §5.4: session name and pane title are
// event-driven VDOM (re-rendered only when pane:focus fires), while the
// clock is a display list the native side diffs and repaints directly,
// avoiding full VDOM reconciliation for a widget that ticks continuously.

function formatClock(now) {
    const hh = String(now.getHours()).padStart(2, '0');
    const mm = String(now.getMinutes()).padStart(2, '0');
    return `${hh}:${mm}`;
}

export function activate(context) {
    const state = {
        sessionName: '',
        paneTitle: '',
    };

    const StatusBarView = {
        render() {
            return {
                type: 'div',
                props: { id: 'status-bar' },
                style: { flexDirection: 'row', justifyContent: 'space-between', gap: '8px' },
                children: [
                    {
                        type: 'span',
                        props: { id: 'session-name' },
                        children: [state.sessionName],
                    },
                    {
                        type: 'span',
                        props: { id: 'pane-title' },
                        children: [state.paneTitle],
                    },
                    // Placeholder region: the native bridge repaints it from
                    // renderClock() output so the clock never touches the
                    // VDOM diff path. `renderer` names the view method to call.
                    {
                        type: 'display-list',
                        props: { id: 'clock', renderer: 'renderClock' },
                        children: [],
                    },
                ],
            };
        },

        // Display-list pattern (spec §5.4): draw ops instead of a VDOM
        // subtree, one small JSON payload per tick.
        renderClock() {
            return [{ op: 'drawText', text: formatClock(new Date()), x: 0, y: 0 }];
        },
    };

    // pane:focus payloads carry the focused pane's title and owning session,
    // so a single subscription keeps both low-frequency fields current.
    context.mux.subscribe('pane:focus', (pane) => {
        state.sessionName = pane.sessionName || state.sessionName;
        state.paneTitle = pane.title || '';
        // The host injects invalidate() on registered views to request a
        // re-render; event-driven chrome re-renders only when asked.
        StatusBarView.invalidate();
    });

    context.registerChromeView('status-bar', StatusBarView);
}
