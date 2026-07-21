// z3rm-which-key — keybinding hints shown while a prefix chord is pending
// (spec §5.5, on-demand).
//
// The keymap subsystem owns prefix detection; this extension only reacts to
// its events. Hints come with the event so the extension never needs keymap
// read access of its own, keeping it workspace-capability-only.

export function activate(context) {
    const state = {
        visible: false,
        prefix: '',
        // [{ key, label }]
        hints: [],
    };

    const WhichKeyView = {
        // Null root while hidden: on-demand chrome occupies no layout space.
        render() {
            if (!state.visible) {
                return null;
            }
            return {
                type: 'div',
                props: { id: 'which-key' },
                style: { flexDirection: 'column', gap: '4px' },
                children: [
                    {
                        type: 'span',
                        props: { id: 'which-key-prefix' },
                        children: [`${state.prefix} +`],
                    },
                    {
                        type: 'div',
                        props: { id: 'which-key-hints' },
                        style: { flexDirection: 'row', gap: '12px', flexWrap: 'wrap' },
                        children: state.hints.map((hint) => ({
                            type: 'span',
                            props: { id: `hint-${hint.key}`, class: 'hint' },
                            children: [`${hint.key}: ${hint.label}`],
                        })),
                    },
                ],
            };
        },
    };

    // 'prefix' events: { active: true, prefix, hints } when the prefix key is
    // pressed; { active: false } when the chord completes or is cancelled.
    context.keymaps.subscribe('prefix', (event) => {
        state.visible = event.active;
        state.prefix = event.prefix || '';
        state.hints = event.hints || [];
        WhichKeyView.invalidate();
    });

    context.registerChromeView('which-key', WhichKeyView);
}
