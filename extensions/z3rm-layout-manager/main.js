// z3rm-layout-manager — save and apply named layout presets (spec §5.5).
//
// Presets live in memory for the activation's lifetime: persisting them to
// disk would require the `settings` capability, which this extension does
// not declare (§5.6 violations throw at runtime). The current layout is
// tracked from session:layout notifications, so saving never needs an extra
// round trip to the mux server.

export function activate(context) {
    const state = {
        open: false,
        presetName: '',
        currentLayout: null,
        // name -> LayoutTree
        presets: new Map(),
    };

    // SessionLayoutChanged is at-least-once (§15.4); replacing the tracked
    // layout whole is idempotent, so duplicate deliveries are harmless.
    context.mux.subscribe('session:layout', (layout) => {
        state.currentLayout = layout;
    });

    const LayoutManagerView = {
        // Null root keeps the overlay hidden until opened.
        render() {
            if (!state.open) {
                return null;
            }
            return {
                type: 'div',
                props: { id: 'layout-manager' },
                style: { flexDirection: 'column', gap: '4px' },
                children: [
                    {
                        type: 'div',
                        props: { id: 'layout-save' },
                        style: { flexDirection: 'row', gap: '4px' },
                        children: [
                            {
                                type: 'input',
                                props: {
                                    id: 'preset-name',
                                    placeholder: 'Preset name',
                                    value: state.presetName,
                                    onChange: { command: 'z3rm.layout.name' },
                                },
                                children: [],
                            },
                            {
                                type: 'button',
                                props: { onClick: { command: 'z3rm.layout.save' } },
                                children: ['save current layout'],
                            },
                        ],
                    },
                    {
                        type: 'div',
                        props: { id: 'preset-list' },
                        children: Array.from(state.presets.keys()).map((name) => ({
                            type: 'button',
                            props: {
                                id: `preset-${name}`,
                                class: 'preset-row',
                                onClick: { command: 'z3rm.layout.load', args: [name] },
                            },
                            children: [name],
                        })),
                    },
                ],
            };
        },
    };

    context.commands.register('z3rm.layout.name', (name) => {
        state.presetName = name || '';
    });

    context.commands.register('z3rm.layout.save', () => {
        const name = state.presetName.trim() || 'default';
        if (state.currentLayout) {
            state.presets.set(name, state.currentLayout);
        }
        // Surface the panel so the user sees the preset list update.
        state.open = true;
        LayoutManagerView.invalidate();
    });

    // With a preset name (from a preset row) applies that preset directly;
    // without one, opens the chooser panel.
    context.commands.register('z3rm.layout.load', (name) => {
        if (name && state.presets.has(name)) {
            context.mux.applyLayout(state.presets.get(name));
            state.open = false;
            LayoutManagerView.invalidate();
            return;
        }
        state.open = true;
        LayoutManagerView.invalidate();
    });

    context.registerChromeView('layout-manager', LayoutManagerView);
}
