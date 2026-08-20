import { AdaptToWebComponent } from "@proto.ui/adapter-web-component";
import { definePrototype } from "@proto.ui/core";
import { button } from "@proto.ui/prototypes-base/button";
import {
  dialogClose,
  dialogContent,
  dialogDescription,
  dialogMask,
  dialogRoot,
  dialogTitle,
  dialogTrigger,
} from "@proto.ui/prototypes-base/dialog";
import {
  asSelectTrigger,
  selectContent,
  selectItem,
  selectRoot,
  selectValue,
} from "@proto.ui/prototypes-base/select";
import { tabsContent, tabsList, tabsRoot, tabsTrigger } from "@proto.ui/prototypes-base/tabs";
import { toggle } from "@proto.ui/prototypes-base/toggle";

type AccessibleSelectTriggerProps = {
  disabled?: boolean;
  a11yLabel?: string;
};

const accessibleSelectTrigger = definePrototype<AccessibleSelectTriggerProps>({
  name: "z3rm-select-trigger",
  modules: asSelectTrigger.modules,
  setup(def) {
    asSelectTrigger();
    def.props.define({ a11yLabel: { type: "string", empty: "fallback" } });
    def.props.setDefaults({ a11yLabel: "Select an option" });
    const a11yLabel = def.state.string("a11yLabel", "Select an option");
    def.a11y.name(a11yLabel);
    const syncLabel = (label: string | undefined) => {
      a11yLabel.set(label || "Select an option", "reason: accessible select label sync");
    };
    def.lifecycle.onMounted((run) => syncLabel(run.props.get().a11yLabel));
    def.props.watch(["a11yLabel"], (_run, next) => syncLabel(next.a11yLabel));
  },
});

AdaptToWebComponent(button, { registerAs: "proto-ui-base-button" });
AdaptToWebComponent(dialogRoot, { registerAs: "proto-ui-base-dialog-root" });
AdaptToWebComponent(dialogTrigger, { registerAs: "proto-ui-base-dialog-trigger" });
AdaptToWebComponent(dialogMask, { registerAs: "proto-ui-base-dialog-mask" });
AdaptToWebComponent(dialogContent, { registerAs: "proto-ui-base-dialog-content" });
AdaptToWebComponent(dialogTitle, { registerAs: "proto-ui-base-dialog-title" });
AdaptToWebComponent(dialogDescription, { registerAs: "proto-ui-base-dialog-description" });
AdaptToWebComponent(dialogClose, { registerAs: "proto-ui-base-dialog-close" });
AdaptToWebComponent(selectRoot, { registerAs: "proto-ui-base-select-root" });
AdaptToWebComponent(accessibleSelectTrigger, { registerAs: "proto-ui-base-select-trigger" });
AdaptToWebComponent(selectValue, { registerAs: "proto-ui-base-select-value" });
AdaptToWebComponent(selectContent, { registerAs: "proto-ui-base-select-content" });
AdaptToWebComponent(selectItem, { registerAs: "proto-ui-base-select-item" });
AdaptToWebComponent(tabsRoot, { registerAs: "proto-ui-base-tabs-root" });
AdaptToWebComponent(tabsList, { registerAs: "proto-ui-base-tabs-list" });
AdaptToWebComponent(tabsTrigger, { registerAs: "proto-ui-base-tabs-trigger" });
AdaptToWebComponent(tabsContent, { registerAs: "proto-ui-base-tabs-content" });
AdaptToWebComponent(toggle, { registerAs: "proto-ui-base-toggle" });
