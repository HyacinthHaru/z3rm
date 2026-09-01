//! §3.3 OSC 133 command boundaries: where each command started and ended.
//!
//! The shell reports these as markers; the server keeps them per pane and hands
//! them back as `CommandRange`s carrying tmux line numbers (viewport row 0,
//! negative into history). Turning those markers into a range a caller can
//! address is the same job for the CLI's `capture-pane -c` and for the GUI
//! jumping between prompts, so it lives here rather than in either surface.

use mux_protocol::proto::CommandRange;

/// 一条命令输出所占的行区间。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSpan {
    /// 起点可用。`end` 为 `None` 表示命令还在跑，取到可见区末尾。
    Located { start: i64, end: Option<i64> },
    /// marker 记下来了，但那些行已经不可寻址。
    Unaddressable,
    /// shell 根本没报告过这条命令从哪儿开始。
    Unmarked,
}

/// 求一条命令的输出区间。
///
/// 缺 marker 时退到更靠前的那个 (`C` → `B` → `A`)：多带上命令行甚至提示符，
/// 总好过漏掉真正的输出。行号本身不可用时返回 `Unaddressable`，绝不猜一个。
pub fn command_output_span(command: &CommandRange) -> CommandSpan {
    let starts = [&command.output_start, &command.command, &command.prompt];
    let Some(start) = starts
        .iter()
        .find_map(|marker| marker.as_ref().and_then(|marker| marker.line))
    else {
        return if starts.iter().any(|marker| marker.is_some()) {
            CommandSpan::Unaddressable
        } else {
            CommandSpan::Unmarked
        };
    };

    let end = match &command.command_end {
        // D 落在第 0 列意味着 shell 已经换到新的一行才报告结束，那一行不属于
        // 输出；落在行中间则说明它还在输出的最后一行上。
        Some(marker) => match marker.line {
            Some(line) if marker.column == 0 => Some(line.saturating_sub(1)),
            Some(line) => Some(line),
            // 起点找得到而终点找不到，说明这一对配不上了；capture 到可见区末尾
            // 会把后面无关的输出一起带上。
            None => return CommandSpan::Unaddressable,
        },
        None => None,
    };

    CommandSpan::Located { start, end }
}

/// 一条命令的提示符所在行 —— 跳转要落在提示符上, 而不是输出的第一行:
/// 用户想看的是"这条命令是什么", 那行字在提示符上。
///
/// `A` 缺失时退到 `B`、再退到 `C`, 与 [`command_output_span`] 同一个原则:
/// 落在稍前的位置总好过跳不过去。
pub fn command_prompt_line(command: &CommandRange) -> Option<i64> {
    [&command.prompt, &command.command, &command.output_start]
        .iter()
        .find_map(|marker| marker.as_ref().and_then(|marker| marker.line))
}

/// 从 `from_line` 出发, 朝一个方向找下一条命令的提示符行。
///
/// `commands` 不必有序 —— 服务端按 marker sequence 给, 而 resize 或 clear 之后
/// 行号可能不再单调。所以这里按行号本身挑最近的一条, 而不是按数组下标前后取。
/// 找不到就返回 `None`: 已经在最上/最下一条命令上时, 什么都不该动。
pub fn adjacent_prompt_line(
    commands: &[CommandRange],
    from_line: i64,
    backward: bool,
) -> Option<i64> {
    commands
        .iter()
        .filter_map(command_prompt_line)
        .filter(|line| {
            if backward {
                *line < from_line
            } else {
                *line > from_line
            }
        })
        .reduce(|nearest, line| {
            // 朝上找最大的那个 (最靠近), 朝下找最小的那个。
            if backward {
                nearest.max(line)
            } else {
                nearest.min(line)
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mux_protocol::proto::CommandMarker;

    fn marker(line: i64, column: u32) -> Option<CommandMarker> {
        Some(CommandMarker {
            line: Some(line),
            column,
        })
    }

    fn command(id: u64, prompt: Option<i64>) -> CommandRange {
        CommandRange {
            id,
            prompt: prompt.and_then(|line| marker(line, 0)),
            ..Default::default()
        }
    }

    #[test]
    fn a_span_falls_back_to_the_earlier_marker() {
        let with_output = CommandRange {
            id: 1,
            output_start: marker(-5, 0),
            command: marker(-6, 2),
            prompt: marker(-6, 0),
            ..Default::default()
        };
        assert_eq!(
            command_output_span(&with_output),
            CommandSpan::Located {
                start: -5,
                end: None
            }
        );

        // 没有 C 时退到 B, 宁可多带上命令行也不漏掉输出。
        let without_output = CommandRange {
            id: 2,
            command: marker(-6, 2),
            prompt: marker(-6, 0),
            ..Default::default()
        };
        assert_eq!(
            command_output_span(&without_output),
            CommandSpan::Located {
                start: -6,
                end: None
            }
        );
    }

    #[test]
    fn a_marker_without_a_line_is_unaddressable_not_guessed() {
        let retired = CommandRange {
            id: 3,
            prompt: Some(CommandMarker {
                line: None,
                column: 0,
            }),
            ..Default::default()
        };
        assert_eq!(command_output_span(&retired), CommandSpan::Unaddressable);
        assert_eq!(command_output_span(&CommandRange::default()), CommandSpan::Unmarked);
    }

    #[test]
    fn an_end_marker_in_column_zero_excludes_its_own_line() {
        // shell 换行之后才报告结束, 那一行是新的提示符, 不属于这条命令的输出。
        let wrapped = CommandRange {
            id: 4,
            output_start: marker(-9, 0),
            command_end: marker(-4, 0),
            ..Default::default()
        };
        assert_eq!(
            command_output_span(&wrapped),
            CommandSpan::Located {
                start: -9,
                end: Some(-5)
            }
        );

        let inline = CommandRange {
            id: 5,
            output_start: marker(-9, 0),
            command_end: marker(-4, 12),
            ..Default::default()
        };
        assert_eq!(
            command_output_span(&inline),
            CommandSpan::Located {
                start: -9,
                end: Some(-4)
            }
        );
    }

    #[test]
    fn jumping_finds_the_nearest_prompt_in_the_asked_direction() {
        let commands = [
            command(1, Some(-40)),
            command(2, Some(-20)),
            command(3, Some(-5)),
        ];

        assert_eq!(adjacent_prompt_line(&commands, -20, true), Some(-40));
        assert_eq!(adjacent_prompt_line(&commands, -20, false), Some(-5));
        assert_eq!(adjacent_prompt_line(&commands, 0, true), Some(-5));
    }

    /// 已经在最上/最下一条命令上时不该跳: 静静不动比跳到别处更容易理解。
    #[test]
    fn jumping_past_the_last_prompt_stays_put() {
        let commands = [command(1, Some(-40)), command(2, Some(-20))];

        assert_eq!(adjacent_prompt_line(&commands, -40, true), None);
        assert_eq!(adjacent_prompt_line(&commands, -20, false), None);
    }

    /// resize 或 clear 之后服务端给的顺序不再对应行号顺序, 所以挑的是行号最近
    /// 的那条, 而不是数组里相邻的那条。
    #[test]
    fn jumping_uses_line_numbers_not_the_order_they_arrived_in() {
        let commands = [
            command(1, Some(-5)),
            command(2, Some(-40)),
            command(3, Some(-20)),
        ];

        assert_eq!(adjacent_prompt_line(&commands, 0, true), Some(-5));
        assert_eq!(adjacent_prompt_line(&commands, -5, true), Some(-20));
        assert_eq!(adjacent_prompt_line(&commands, -40, false), Some(-20));
    }

    /// 行号被退休 (resize / clear) 的命令不该把跳转吞掉。
    #[test]
    fn commands_without_a_line_are_skipped_rather_than_jumped_to() {
        let commands = [command(1, Some(-30)), command(2, None), command(3, Some(-10))];

        assert_eq!(adjacent_prompt_line(&commands, 0, true), Some(-10));
        assert_eq!(adjacent_prompt_line(&commands, -10, true), Some(-30));
    }
}
