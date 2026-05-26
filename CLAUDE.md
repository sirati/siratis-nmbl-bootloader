you must never edit this file.

Always start subagents with background param = true otherwise you operate sequentially!

Never use the following bash commands: timeout, sleep, head, tail
Instead always spawn programs inside a monitor. You must also always spawn a second monitor to wake you every 30s 1m 5m or 10m. Often programs hang, if you do not spawn a waker, you never wake up again. 

To debug VMs use ./vm-serial-man-rs/

Always spawn VMs in a Sonnet 1million subagent, inspecting VM state would takes too much context window otherwise! 

