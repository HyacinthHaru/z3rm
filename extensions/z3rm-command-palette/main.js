// z3rm-command-palette — on-demand filterable command list (spec §5.5).
//
// The palette mirrors the live command registry instead of keeping its own
// action list: every command any extension registers via
// context.commands.register becomes selectable automatically. Its own
// interactions (filter, select, close) are registered as commands too, so
// they are bindable and discoverable like anything else.

export function activate(context) {
    const state = {
        open: false,
        query: '',
        selected: 0,
        // [{ id, label }]
        entries: [],
    };

    function refreshEntries() {
        const query = state.query.toLowerCase();
        state.entries = context.commands
            .list()
            .filter(
                (entry) =>
                    entry.id.toLowerCase().includes(query) ||
                    entry.label.toLowerCase().includes(query)
            );
        if (state.selected >= state.entries.length) {
            state.selected = Math.max(0, state.entries.length - 1);
        }
    }

    const CommandPaletteView = {
        // A null root keeps the overlay hidden; the native bridge treats
        // null as "not visible" for on-demand chrome.
        render() {
            if (!state.open) {
                return null;
            }
            return {
                type: 'div',
                props: { id: 'command-palette' },
                style: { flexDirection: 'column', width: '480px' },
                children: [
                    {
                        type: 'input',
                        props: {
                            id: 'palette-query',
                            placeholder: 'Type a command…',
                            value: state.query,
                            // onChange dispatches the command with the current
                            // text as its argument (bridge convention).
                            onChange: { command: 'z3rm.command-palette.filter' },
                        },
                        children: [],
                    },
                    {
                        type: 'div',
                        props: { id: 'palette-list' },
                        children: state.entries.map((entry, index) => ({
                            type: 'div',
                            props: {
                                id: `palette-entry-${entry.id}`,
                                class: index === state.selected ? 'entry selected' : 'entry',
                                // Direct command click: the native bridge
                                // resolves the owner from the global registry,
                                // so entries from other extensions execute
                                // exactly once, on the right extension.
                                onClick: { command: entry.id },
                            },
                            children: [entry.label],
                        })),
                    },
                ],
            };
        },
    };

    function open() {
        state.open = true;
        state.query = '';
        refreshEntries();
        CommandPaletteView.invalidate();
    }

    function close() {
        state.open = false;
        CommandPaletteView.invalidate();
    }

    context.commands.register('z3rm.command-palette.open', open);
    context.commands.register('z3rm.command-palette.close', close);
    context.commands.register('z3rm.command-palette.filter', (query) => {
        state.query = query || '';
        refreshEntries();
        CommandPaletteView.invalidate();
    });
    context.commands.register('z3rm.command-palette.select', (commandId) => {
        close();
        if (commandId) {
            context.commands.execute(commandId);
        }
    });

    context.keymaps.bind('ctrl+shift+p', 'z3rm.command-palette.open');

    context.registerChromeView('command-palette', CommandPaletteView);
}
