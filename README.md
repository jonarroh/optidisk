# optidisk

Encuentra lo que más pesa en tu disco y bórralo desde la terminal. Hecho para Windows.

## Compilar

```powershell
cargo build --release
.\target\release\optidisk.exe
```

`rust-toolchain.toml` fija el toolchain `windows-gnu`, porque no requiere tener instalado Visual Studio.
Para usarlo desde cualquier carpeta: `cargo install --path .`

## Uso

```text
optidisk [RUTA] [-t N] [--print N]
```

| Ejemplo | Qué hace |
|---|---|
| `optidisk` | Analiza la unidad del sistema |
| `optidisk D:\` | Analiza otra unidad o carpeta |
| `optidisk C:\ --print 50` | Sin TUI: imprime los 50 archivos más grandes |
| `optidisk -t 16` | Fija el número de hilos del escaneo |

Ejecútalo como administrador para medir también las carpetas protegidas; sin permisos, se marcan con `!` y
su tamaño cuenta como un mínimo.

## Teclas

| Tecla | Acción |
|---|---|
| `↑ ↓` `j k`, `RePág AvPág`, `Inicio Fin` | Moverse |
| `Enter` `→` | Abrir la carpeta (en el top: ir a la carpeta del archivo) |
| `←` `Retroceso` | Subir |
| `Tab` | Alternar explorador y los 1000 archivos más grandes de todo el disco |
| `Espacio` / `Esc` | Marcar / limpiar marcas |
| `d` | Mover a la papelera lo marcado o seleccionado (confirma con `y`) |
| `D` | Borrar definitivamente (confirma escribiendo `borrar`) |
| `r` | Volver a escanear |
| `?` / `q` | Ayuda / salir |

## Seguridad al borrar

- **Papelera por defecto.** Recuperable, pero no libera espacio hasta vaciarla. Si algo no cabe en la
  papelera, da error en lugar de borrarse sin aviso; ahí decides si usar `D`.
- **Rutas protegidas.** Nunca se borran: raíces de unidad, `Windows` y su contenido, `Program Files`,
  `ProgramData`, tu perfil y sus carpetas principales (`Documents`, `AppData`…), archivos como
  `pagefile.sys` y sus carpetas contenedoras. El contenido de `Program Files` o `Downloads` sí se puede borrar.
- **Enlaces.** Los symlinks y junctions pesan 0 y no se siguen: al borrarlos se quita el enlace, nunca su
  destino.
- **Avisos.** La confirmación advierte de los `.vhdx` (borrarlos destruye una distro de WSL o los datos de
  Docker) y de lo que está en OneDrive (se borra también en la nube).
- **Borrados a medias.** Si un archivo está en uso, se vuelve a medir la carpeta para que los tamaños sigan
  coincidiendo con el disco, y se informa de qué falló.

- **Progreso.** `D` muestra una barra con los archivos y bytes borrados, medidos sobre lo que queda en disco.
  La papelera mueve cada carpeta de una vez, así que muestra actividad y tiempo, sin porcentaje.

## Qué mide y qué no

- El **tamaño en disco**: los archivos de OneDrive que están sólo en la nube cuentan 0 y los comprimidos o
  dispersos, lo que ocupan realmente.
- Los **enlaces duros cuentan una vez por enlace**, porque detectarlos exige abrir cada archivo. Por eso
  `C:\Windows\WinSxS` aparece más grande de lo que es y la suma puede superar lo usado en el volumen.
- Los archivos normales se miden por su tamaño lógico, sin redondear al clúster.
- Sólo se ha probado en Windows.

## Desarrollo

`cargo test` cubre el escaneo, el árbol, las reglas de protección, el borrado real en carpetas temporales
(junctions y papelera incluidas) y las pantallas con un backend de prueba.
