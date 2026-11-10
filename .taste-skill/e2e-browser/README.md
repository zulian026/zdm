# e2e-browser — verifikasi intercept extension ZDM di laptop

Skrip yang dipakai untuk verifikasi end-to-end extension browser v1.2.0
(2026-11-10 19:10–19:20 WIB). Dijalankan di laptop (cachyos-x8664), bukan di VM.

## run_e2e_chromium.sh

Alur: halaman uji auto-download file 5 MiB (http://127.0.0.1:18765) di
Chromium headless dengan `--load-extension=browser/chromium` → extension
membatalkan download browser → native host menulis URL ke queue.json →
`/usr/bin/zdm` (sudah jalan) mengunduh file → cek sha256 cocok.

Catatan penting (temuan saat e2e):
- Chrome mencari manifest native host di bawah `--user-data-dir` yang
  dipakai, BUKAN di `~/.config/chromium/`. Profil headless sementara
  tidak punya manifest → skrip menyalin
  `~/.config/chromium/NativeMessagingHosts/com.zdm.native.json` ke
  `<profil-sementara>/NativeMessagingHosts/` sebelum menjalankan Chromium.
  Pada pemakaian normal (profil default) langkah ini tidak diperlukan —
  `browser/install.sh` sudah mendaftarkan manifest di sana.
- File `.bin` dikategorikan ZDM ke subfolder `Programs/`, jadi file hasil
  unduhan dicek di `~/Downloads/ZDM/Programs/test-5mib.bin`, bukan
  langsung di `~/Downloads/ZDM/`.

## run_e2e_firefox.sh

Alur yang sama di Firefox 159 headless via Selenium 4.52 + geckodriver
0.37.1 (venv di `/tmp/zdm-e2e-browser/venv` di laptop; `pip install
selenium` di venv karena pip sistem Arch menolak PEP 668). Add-on
di-install sebagai temporary add-on
(`install_addon(..., temporary=True)` → id `zdm@example.local`).

Catatan penting:
- `opts.headless = True` saja TIDAK cukup — geckodriver menjalankan
  Firefox tanpa headless sehingga gagal `no DISPLAY environment variable
  specified`. Perlu `MOZ_HEADLESS=1` di environment + argumen `-headless`.
- Firefox 159 gagal `Could not find profile folder` bila direktori
  profil belum ada — skrip membuat `/tmp/zdm-e2e-browser/ff-profile`
  dulu dan memberikannya sebagai `FirefoxProfile` eksplisit.

Kedua skrip: mem-backup `~/.config/zdm/settings.json`, mematikan
`ask_before_download` + `clamav_scan` selama tes, lalu memulihkan
settings byte-for-byte (dicek sha256) dan mematikan ZDM + server HTTP
sesudahnya. File uji dihapus; baris riwayat di downloads.db tetap ada
(id 11 Chromium, id 12 Firefox).
