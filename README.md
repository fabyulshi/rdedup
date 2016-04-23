# rdedup

<p align="center">
  <a href="https://travis-ci.org/dpc/rdedup">
      <img src="https://img.shields.io/travis/dpc/rdedup/master.svg?style=flat-square" alt="Travis CI Build Status">
  </a>
  <a href="https://crates.io/crates/rdedup">
      <img src="http://meritbadge.herokuapp.com/rdedup?style=flat-square" alt="crates.io">
  </a>
  <a href="https://gitter.im/dpc/dpc">
      <img src="https://img.shields.io/badge/GITTER-join%20chat-green.svg?style=flat-square" alt="Gitter Chat">
  </a>
  <br>
</p>


## Introduction

**Warning: alpha/prototype quality software ahead**

`rdedup` is a tool providing data deduplication with compression and public key
encryption written in Rust programming language. It's useful for backups.

### My use case

I use [rdup][rdup] to make backups, and also use [syncthing][syncthing] to
duplicate my backups over a lot of systems. Some of them are more trusted
(desktops with disk-level encryption, firewalls, stored in the vault etc.), and
some not so much (semi-personal laptops, phones etc.)

As my backups tend to contain a lot of shared data (even backups taken on
different systems), it makes perfect sense to deduplicate them.

However I'm paranoid and I don't want one of my hosts being physically or
remotely compromised, give access to data inside all my backups from all my
systems.  Existing deduplication software like [ddar][ddar] or
[zbackup][zbackup] provide encryption, but only symmetrical ([zbackup
issue][zbackup-issue], [ddar issue][ddar-issue]) which means you have to share
the same key on all your hosts and one compromised system compromises all your
backups.

To fill the missing piece in my master backup plan, I've decided to write it
myself using my beloved Rust programming language. That's how `rdedup` started.

## How it works

`rdedup` works very much like [zbackup][zbackup] and other deduplication software.

`rdedup` uses a special format to use a given directory as a deduplication
storage.

When saving data, `rdedup` will split it into smaller pieces (chunks) using
rolling sum algorithm, and store each chunk under unique name (sha256 digest).
Then the whole backup will be described as a list of chunks (their ids).

When restoring data, `rdedup` will read the list of chunks and recreate the
original data.

Thanks to this chunking scheme, when saving frequently similar data, a lot of
common chunks will be reused, saving space.

What makes `rdedup` unique, is that every time new storage directory is created, a pair
of keys (public and secret) is being generated. Public key is saved in the
storage directory itself, while secret key is supposed to be written down or stored
securely in outside location.

Every `rdedup` saves a new chunk of data it's encrypted with public key so it can
only be decrypted using the corresponding secret key. This way new backups can
be created, with full deduplication, while accessing the data requires the
private key.

### Details

* [bup][bup] method is used to split files into chunks
* sha256 is used to identify chunks
* [libsodium][libsodium]'s [sealed boxes][libsodium-sealed-boxes-doc] are used for encryption/decryption:
  * ephemeral keys are used for sealing
  * chunk digest is used as nonce


## Usage

```
rdedup init
```

will create a `backup` subdirectory in current directory and generate a keypair
used for encryption.

```
rdedup save <name>
```

will save any data given on standard input under given *name*.

```
rdedup restore <name>
```

will write on standard output data previously stored under given *name*


In combination with [rdup][rdup] this can be used to store and restore your backup like this:

```
rdup -x /dev/null "$HOME" | rdedup save home
rdedup load home | rdup-up "$HOME.restored"
```

[bup]: https://github.com/bup/bup/
[rdup]: https://github.com/miekg/rdup
[syncthing]: https://syncthing.net
[zbackup]: http://zbackup.org/
[zbackup-issue]: https://github.com/zbackup/zbackup/issues/109
[ddar]: https://github.com/basak/ddar/
[ddar-issue]: https://github.com/basak/ddar/issues/10
[libsodium-sealed-boxes-doc]: https://download.libsodium.org/doc/public-key_cryptography/sealed_boxes.html
[libsodium]: https://github.com/jedisct1/libsodium

