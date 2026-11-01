// The page's answer to the `storage` family: the browser's localStorage.
// `call` gets the name and args a program passed to host.call, and what it
// returns (or a promise of it) is the reply.
export const storage = {
  call(name, args) {
    switch (name) {
      case 'storage/set':
        localStorage.setItem(args.key, args.value);
        return true;
      case 'storage/get': {
        const value = localStorage.getItem(args.key);
        if (value === null) throw new Error(`no key '${args.key}'`);
        return value;
      }
      default:
        throw new Error(`no call '${name}'`);
    }
  },
};
