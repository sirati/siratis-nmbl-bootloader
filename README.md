This project is under development and large broken or untested.

It for now only aims to support NixOS. 

No more boot loader is the concept of using Linux as a bootloader. This allows for more complex setups than possible with traditional boot loaders. Most importantly being able to boot any disk configuration that would be mountable by Linux.

It also allows for rich behaviours such as detecting when a Linux boot failed, and only displaying boot options in such a case or during frequent rebooting. It also may allow limited local and remote shell access should booting fail.  