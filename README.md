# PODCAZ

Consola de podcasting para Windows escrita en Rust: mezclas y grabas en directo, y al terminar
exportas el episodio ya mezclado y masterizado.

- **De 0 a 4 pistas de voz** (se elige en la barra superior, «Voces») con presets de EQ + dinámica
  (filtro paso alto, puerta, EQ de 4 bandas, compresor). Con 0 voces se puede grabar solo música y sampler.
- **Pista de música** con setlist por sesión y **ducking automático** cuando alguien habla.
- **Sampler de 16 pads** que se disparan por **MIDI in** o con el **teclado** (`1234 / QWER / ASDF / ZXCV`).
  Cada pad se puede **mapear a la nota MIDI que quieras** (a mano o con «Aprender del controlador»).
  Los disparos se guardan en un fichero **`.mid`** junto a cada toma.
- **Grabación por pistas separadas** (stems WAV de 24 bit) y **edición al exportar**:
  recorte de inicio/fin, volumen por pista, fundido de salida y **curvas de subidas y bajadas de
  volumen** editables sobre la línea de tiempo (por pista y de la mezcla completa).
- **Exportación a varios ficheros**: mixdown masterizado (WAV/MP3), una pista WAV por voz y el MIDI
  de los pads. El mixdown se normaliza en sonoridad (LUFS) y se limita con techo a -1 dB de pico real.
- Audio por **ASIO** o **WASAPI**.
- **Modo claro y oscuro** (botón ☀/🌙 arriba a la derecha; la elección se recuerda) e icono propio:
  un micrófono con orejas de gato y ondas de sonido, dibujado por código.

## Compilar (Windows 10/11, 64 bit)

### 1. Requisitos

| Qué | Para qué | Instalación |
|---|---|---|
| Rust (rustup) | compilar | `winget install --id Rustlang.Rustup -e` |
| Visual Studio Build Tools con «Desarrollo para escritorio con C++» | enlazador, LAME, SDK de ASIO | `winget install --id Microsoft.VisualStudio.2022.BuildTools -e` y marcar la carga de C++ |
| LLVM | solo para ASIO (genera los bindings del SDK) | `winget install --id LLVM.LLVM -e` |

Además, para ASIO hay que definir la variable `LIBCLANG_PATH`:

```bat
setx LIBCLANG_PATH "C:\Program Files\LLVM\bin"
```

El script `setup.ps1` comprueba todo esto y, con `-Install`, instala lo que falte y crea la variable:

```powershell
powershell -ExecutionPolicy Bypass -File .\setup.ps1 -Install
```

Después **cierra y vuelve a abrir VS Code** para que vea las variables nuevas.

El SDK de ASIO no hay que descargarlo a mano: la primera compilación lo baja de steinberg.net
(hace falta conexión esa vez).

### 2. Abrir en VS Code

1. `Archivo > Abrir carpeta…` y elige esta carpeta.
2. Acepta las extensiones recomendadas (rust-analyzer, C/C++ para depurar).
3. `Ctrl+Shift+B` lanza la tarea por defecto: **compilar y ejecutar (ASIO + WASAPI)**.

O desde el terminal:

```bat
cargo run --release -p podcaz
```

La primera compilación tarda varios minutos. El ejecutable queda en `target\release\podcaz.exe`.

### Si la compilación con ASIO falla

Compila sin ASIO (solo WASAPI). No necesita LLVM y sirve para probar todo lo demás:

```bat
cargo run --release -p podcaz --no-default-features --features mp3
```

Es la tarea «PODCAZ: 2 - compilar y ejecutar SIN ASIO». Los fallos típicos de ASIO son
`LIBCLANG_PATH` sin definir o que falte la carga de C++ de Visual Studio.

### Tests

```bat
cargo test -p podcaz-core --release
```

## Uso rápido

1. **Audio**: elige `ASIO` y el driver de tu interfaz (o `WASAPI` y entrada/salida) y pulsa
   **Iniciar audio**. Con ASIO el tamaño de buffer se ajusta en el panel del driver; deja «driver».
2. **Voces**: en «Voces» elige cuántas pistas de voz usas (0–4), el canal de entrada de cada micro
   y un preset. Sube la *ganancia de entrada* hasta que la voz marque en la zona verde alta.
3. **Música**: añade temas al setlist. Doble clic para reproducir. El ducking baja la música
   cuando hay voz; ajusta cuánto baja y la sensibilidad.
4. **Sampler**: clic derecho en un pad para asignarle un sonido. Se dispara con clic, con su tecla
   o con su nota MIDI (`Esc` para parar todo).
   - **Mapeo MIDI**: por defecto el pad 1 es la nota 36 y los demás siguen en orden (se cambia con
     «Nota base»). Para mapear un pad a otra nota, clic derecho en el pad y **«Aprender del
     controlador…»** (después toca la tecla o pad de tu controlador), o escribe la nota. Cada nota
     dispara un solo pad. El mapeo se guarda con la sesión y es el que se escribe en el `.mid`.
5. **GRABAR** / **PARAR**. Cada toma se guarda en `takes\<fecha-hora>\`.
6. **Exportar…**: elige la toma, recorta, ajusta volúmenes, elige sonoridad y qué ficheros quieres.
7. **Subidas y bajadas de volumen** (en la ventana de exportar): curvas dibujadas sobre la forma
   de onda. Clic para añadir un punto, arrastrar para moverlo, clic derecho (o `Supr`) para
   borrarlo, rueda del ratón para el zoom. Hay una curva por pista y otra de la **mezcla completa**.
   El **ducking de la música se graba como curva** (la música se guarda limpia), así que puedes
   suavizarlo, quitarlo o subir la música en un tramo. «Restaurar lo grabado» vuelve al original.
   Los cambios se guardan solos en la toma y se pueden retocar otro día.

«Escuchar las voces» envía los micros a la salida: úsalo solo con auriculares.

### Carpeta de una sesión

```text
Documentos\PODCAZ\Mi podcast\
  session.json                 voces, presets, setlist, pads (con su nota MIDI), dispositivos
  takes\20261111-161700\
    voice1.wav … voice4.wav    solo las voces activas (mono, 24 bit, ya procesadas)
    music.wav                  música SIN ducking (estéreo)
    sampler.wav                sampler (estéreo)
    pads.mid                   disparos del sampler (formato 0, 120 bpm, con el mapeo de la toma)
    pad_events.json            los mismos disparos, para recortarlos al exportar
    mix.json                   subidas y bajadas de volumen (editable; incluye el ducking)
    mix.auto.json              copia original de lo grabado, para poder restaurarlo
    take.json
  export\episodio\             una carpeta por exportación
    episodio - mixdown.wav     mezcla masterizada (y .mp3 si lo eliges)
    episodio - voz 1 (Ana).wav una por cada voz grabada: mismo recorte y fundidos que el mixdown,
    episodio - voz 2 (Luis).wav   con su volumen y su curva, sin masterizar
    episodio - midi.mid        disparos de pads, desplazados según el recorte
```

Las tomas grabadas con versiones anteriores siguen funcionando: llevan el ducking fijo en
`music.wav` y no tienen curva de ducking, y su MIDI se copia sin recortar.

## Estructura del código

```text
crates/podcaz-core/     motor, sin dependencias de hardware (con tests)
  dsp.rs       biquads, puerta, compresor, limitador con anticipación
  presets.rs   presets de voz
  engine.rs    mezclador en tiempo real: voces, ducking, sampler, stems
  recorder.rs  hilo que escribe los stems a disco y guarda el ducking como curva
  automation.rs envolventes de volumen (puntos en dB) y su fichero mix.json
  decode.rs    carga de mp3/wav/flac/ogg/m4a y remuestreo
  master.rs    etapa previa al máster y objetivos de sonoridad
  export.rs    mezcla offline, recorte, masterizado; mixdown WAV/MP3, pistas de voz y MIDI
  midifile.rs  escritura de .mid
  session.rs   sesión en JSON
crates/podcaz-app/      aplicación de Windows
  audio_io.rs  cpal (ASIO/WASAPI)
  midi_in.rs   entrada MIDI (midir): tabla nota -> pad y «aprender nota»
  app.rs       interfaz egui y lógica (incluye el editor de curvas de volumen)
```

Para añadir o retocar un preset de voz basta con editar la tabla de `presets.rs`.

## Qué está verificado y qué no

Verificado al preparar este proyecto:

- Los tests (`cargo test --workspace --release`) pasan: DSP, ducking, grabación de stems y `.mid`
  con mapeo, remuestreo, número de voces de la sesión, tabla MIDI y aprendizaje de notas,
  envolventes, y exportaciones completas que comprueban la sonoridad objetivo, el techo de pico,
  los ficheros generados (mixdown, voces, MIDI recortado), que las curvas cambian el audio y que
  el ducking grabado se puede conservar o quitar al exportar.
- El proyecto compila y enlaza para Windows **sin ASIO** (compilación cruzada).
- La interfaz arranca y la exportación funciona de extremo a extremo desde la ventana.

Sin verificar, porque requiere una máquina Windows con hardware de audio:

- La compilación **con ASIO** (necesita MSVC + LLVM en Windows).
- La captura y reproducción reales con ASIO/WASAPI y la entrada MIDI.

## Limitaciones conocidas de esta versión

- Las voces se graban **ya procesadas** (preset y volumen aplicados): al exportar se puede
  cambiar el volumen de cada pista, pero no el preset.
- El número de voces se fija al empezar a grabar. El máximo es 4 (`MAX_VOICES`).
- Las curvas de volumen se aplican al exportar y en la escucha de 10 s del editor; no hay
  reproducción continua de la toma con las curvas.
- Una pista de voz sale con su volumen y su curva pero sin masterizar: no suma exactamente el
  mixdown, que además lleva compresión de mezcla, ganancia de sonoridad y limitador.
- La música y los sonidos del sampler se cargan enteros en memoria (un tema de 5 min ≈ 115 MB).
- El recorte es numérico con preescucha de inicio y final; no hay forma de onda ni cortes intermedios.
- Con WASAPI y dispositivos de entrada y salida distintos, ambos deben estar a la misma frecuencia
  de muestreo. Con ASIO entrada y salida usan el mismo driver.
- Una hora de grabación con una voz ocupa unos 2,6 GB (más 0,5 GB por cada voz adicional).

## Licencias de terceros

- El MP3 se codifica con **LAME** (LGPL), enlazado en el ejecutable. Si distribuyes el programa,
  cumple la LGPL o compila sin la feature `mp3`.
- El soporte ASIO usa el **SDK de ASIO de Steinberg**; distribuir binarios con ASIO está sujeto
  a su licencia. ASIO es una marca de Steinberg Media Technologies GmbH.
