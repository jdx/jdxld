# Nix

jdxld includes a flake and overlay for building the standalone development binary. It does not yet
provide a wrapped compiler toolchain.

Build the package from a checkout with:

```sh
nix build
```

Or use the flake directly:

```nix
{
  inputs.jdxld.url = "github:jdx/jdxld";

  outputs = { jdxld, ... }: {
    packages.x86_64-linux.jdxld = jdxld.packages.x86_64-linux.default;
  };
}
```

The supported incremental integration will be provided by mr-boxington rather than a Nix stdenv
adapter.
