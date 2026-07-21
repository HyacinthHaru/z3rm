// z3rm-session-manager — session list with switch/create/kill (spec §5.5).
//
// On-demand overlay: the list is pulled from the mux every time the panel
// opens so it always reflects authoritative server state (§3.10), and each
// mutation re-pulls afterwards rather than patching local copies.

export function activate(context) {
    const state = {
        open: false,
        newName: '',
        // SessionInfo records from context.mux.listSessions():
        // [{ id, name, cwd, clients }]
        sessions: [],
    };

    function refreshSessions() {
        state.sessions = context.mux.listSessions();
    }

    const SessionManagerView = {
        // Null root keeps the overlay hidden until opened.
        render() {
            if (!state.open) {
                return null;
            }
            return {
                type: 'div',
                props: { id: 'session-manager' },
                style: { flexDirection: 'column', gap: '4px' },
                children: [
                    {
                        type: 'div',
                        props: { id: 'session-list' },
                        children: state.sessions.map((session) => ({
                            type: 'div',
                            props: { id: `session-${session.id}`, class: 'session-row' },
                            style: { flexDirection: 'row', gap: '8px' },
                            children: [
                                {
                                    type: 'span',
                                    props: { class: 'session-name' },
                                    children: [`${session.name} (${session.clients} attached)`],
                                },
                                {
                                    type: 'button',
                                    props: {
                                        class: 'session-switch',
                                        onClick: {
                                            command: 'z3rm.session-manager.switch',
                                            args: [session.id],
                                        },
                                    },
                                    children: ['switch'],
                                },
                                {
                                    type: 'button',
                                    props: {
                                        class: 'session-kill',
                                        onClick: {
                                            command: 'z3rm.session-manager.kill',
                                            args: [session.id],
                                        },
                                    },
                                    children: ['kill'],
                                },
                            ],
                        })),
                    },
                    {
                        type: 'div',
                        props: { id: 'session-create' },
                        style: { flexDirection: 'row', gap: '4px' },
                        children: [
                            {
                                type: 'input',
                                props: {
                                    id: 'new-session-name',
                                    placeholder: 'New session name',
                                    value: state.newName,
                                    onChange: { command: 'z3rm.session-manager.name' },
                                },
                                children: [],
                            },
                            {
                                type: 'button',
                                props: {
                                    onClick: { command: 'z3rm.session-manager.create' },
                                },
                                children: ['create'],
                            },
                        ],
                    },
                ],
            };
        },
    };

    function close() {
        state.open = false;
        SessionManagerView.invalidate();
    }

    context.commands.register('z3rm.session-manager.open', () => {
        refreshSessions();
        state.open = true;
        SessionManagerView.invalidate();
    });
    context.commands.register('z3rm.session-manager.close', close);
    context.commands.register('z3rm.session-manager.name', (name) => {
        state.newName = name || '';
    });
    context.commands.register('z3rm.session-manager.create', () => {
        const name = state.newName.trim();
        if (!name) {
            return;
        }
        context.mux.createSession(name);
        state.newName = '';
        refreshSessions();
        SessionManagerView.invalidate();
    });
    context.commands.register('z3rm.session-manager.switch', (sessionId) => {
        // Attaching is the mux's session-switch primitive (§3.3): the server
        // moves this client's window to the target session.
        context.mux.attach(sessionId);
        close();
    });
    context.commands.register('z3rm.session-manager.kill', (sessionId) => {
        context.mux.killSession(sessionId);
        refreshSessions();
        SessionManagerView.invalidate();
    });

    context.registerChromeView('session-manager', SessionManagerView);
}
