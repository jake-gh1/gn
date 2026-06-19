<p align="center">
  <img src="images/news-workflow1.png" alt="gn news coverage workflow" width="100%" />
</p>
<strong>gn</strong> gets, filters, and labels news.
<br />Use <strong><a href="./docs/config.md">local or cloud LLMs</a></strong>.
<br />For details, read <strong><a href="./docs/workflow.md">docs/workflow.md</a></strong>.

---

## Get Started
### Install & Run
**macOS**
```zsh
curl -fsSL https://github.com/jake-gh1/gn/releases/latest/download/gn-installer.sh | sh
```
**Windows**
```zsh
irm https://github.com/jake-gh1/gn/releases/latest/download/gn-installer.ps1 | iex
```

Run `gn` to create or edit the runtime config. For details, see **[docs/config.md](docs/config.md)**.

**Commands**
```zsh
gn msft         # search
   msft / GPUs  
   --model      # change model
```

## Docs

- **[docs/config.md](docs/config.md)**
- **[docs/workflow.md](docs/workflow.md)**

This repository is licensed under the [Apache 2.0 License](LICENSE).
