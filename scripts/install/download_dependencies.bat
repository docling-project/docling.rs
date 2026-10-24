@echo off
rem Windows twin of download_dependencies.sh: fetch the ML models the PDF/image
rem pipeline needs into .models\, so a native (non-WSL) build runs out of the
rem box (the PDF pages themselves are parsed and rendered in pure Rust):
rem
rem     cargo build --release -p docling-cli
rem     target\release\docling-rs paper.pdf
rem
rem Everything lands relative to the repo root (the binary resolves .models\
rem next to the CWD or the executable, no env vars needed). The models come
rem from the same GitHub release the Linux script uses.
rem
rem Usage: scripts\install\download_dependencies.bat [--cli] [--no-asr] [--no-int8] [--force]
rem --cli installs into the cargo install root (%CARGO_INSTALL_ROOT%, else
rem %CARGO_HOME%, else %USERPROFILE%\.cargo) instead of the repo root: its
rem bin\ is where `cargo install docling-cli` puts docling-rs.exe, which looks
rem for .models\ next to itself and one level up, from any directory.
rem Needs curl.exe and tar.exe (both ship with Windows 10 1803+).
rem Integrity: every asset fetched from the release is checked with certutil
rem against its SHA256SUMS before it is moved into place, as the .sh does;
rem DOCLING_RS_MODELS_VERIFY=auto (default) | require | off, see there.
setlocal enabledelayedexpansion

set "BASE_URL=https://github.com/docling-project/docling.rs/releases/download/models-v1"
if not "%DOCLING_RS_MODELS_URL%"=="" set "BASE_URL=%DOCLING_RS_MODELS_URL%"
set "ASR_BASE_URL=https://huggingface.co/onnx-community/whisper-tiny/resolve/main"
set "OCR_EN_URL=https://huggingface.co/SWHL/RapidOCR/resolve/main/PP-OCRv3/en_PP-OCRv3_rec_infer.onnx"
set "EN_DICT_URL=https://raw.githubusercontent.com/PaddlePaddle/PaddleOCR/main/ppocr/utils/en_dict.txt"
set "OCR_DET_URL=https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.9.2/onnx/PP-OCRv6/det/PP-OCRv6_det_small.onnx"
set "OCR_REC_V6_URL=https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.9.2/onnx/PP-OCRv6/rec/PP-OCRv6_rec_small.onnx"
set "OCR_REC_V6_DICT_URL=https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.9.2/paddle/PP-OCRv6/rec/PP-OCRv6_rec_small/ppocrv6_dict.txt"

set WITH_ASR=1
set WITH_INT8=1
set FORCE=0
set INTO_CLI=0
:parse
if "%~1"=="" goto parsed
if "%~1"=="--no-asr"  set WITH_ASR=0
if "%~1"=="--no-int8" set WITH_INT8=0
if "%~1"=="--force"   set FORCE=1
if "%~1"=="--cli"     set INTO_CLI=1
if "%~1"=="--help" (
  echo usage: download_dependencies.bat [--cli] [--no-asr] [--no-int8] [--force]
  exit /b 0
)
shift
goto parse
:parsed

rem repo root = two levels above this script; --cli: the cargo install root
cd /d "%~dp0..\.."
if %INTO_CLI%==1 (
  set "CLI_ROOT=%USERPROFILE%\.cargo"
  if not "%CARGO_HOME%"=="" set "CLI_ROOT=%CARGO_HOME%"
  if not "%CARGO_INSTALL_ROOT%"=="" set "CLI_ROOT=%CARGO_INSTALL_ROOT%"
  if not exist "!CLI_ROOT!" mkdir "!CLI_ROOT!"
  cd /d "!CLI_ROOT!"
  echo --cli: installing into !CLI_ROOT!\.models ^(beside bin\, where cargo install puts docling-rs.exe^)
)

where curl >nul 2>nul || (echo error: curl.exe not found ^(ships with Windows 10 1803+^) & exit /b 1)

rem Never hang forever on a dead mirror: cap the connect phase, abort a
rem transfer stalled below 1 KiB/s for a minute, retry transient failures
rem (docling proper added the same guard, issue #3784). No --retry-delay: a
rem fixed delay bunches every attempt into a few seconds, which is useless
rem against the per-IP 429 the third-party hosts answer shared CI/office
rem egress with - curl's default doubling spreads them out (as in the .sh).
set "CURL_TIMEOUTS=--connect-timeout 30 --speed-limit 1024 --speed-time 60 --retry 5"

rem The release's SHA256SUMS (scripts/ci/models_manifest.sh), fetched once;
rem :verify checks each release asset against it. Without one (a tag that
rem predates the manifest, a mirror without it) downloads go unchecked with a
rem note - or fail under DOCLING_RS_MODELS_VERIFY=require.
set "VERIFY=%DOCLING_RS_MODELS_VERIFY%"
if "%VERIFY%"=="" set "VERIFY=auto"
set "MANIFEST="
if /i not "%VERIFY%"=="off" (
  set "MANIFEST=%TEMP%\docling-rs-SHA256SUMS.%RANDOM%.txt"
  curl -fsSL %CURL_TIMEOUTS% -o "!MANIFEST!" "%BASE_URL%/SHA256SUMS" >nul 2>nul
  if errorlevel 1 (
    del "!MANIFEST!" 2>nul
    set "MANIFEST="
    if /i "!VERIFY!"=="require" (
      echo error: %BASE_URL%/SHA256SUMS unavailable and DOCLING_RS_MODELS_VERIFY=require
      exit /b 1
    )
    echo   ! %BASE_URL% hosts no SHA256SUMS - downloads from it are not integrity-checked
  )
)

if not exist .models\tableformer mkdir .models\tableformer
if not exist .models\asr mkdir .models\asr
if not exist .models\chunk mkdir .models\chunk

echo fetching docling.rs ML dependencies from %BASE_URL%


rem --- required models --------------------------------------------------------
call :fetch "%BASE_URL%/layout_heron.onnx"              .models\layout_heron.onnx           || goto :fail
call :fetch "%BASE_URL%/ocr_rec.onnx"                   .models\ocr_rec.onnx                || goto :fail
call :fetch "%BASE_URL%/ppocr_keys_v1.txt"              .models\ppocr_keys_v1.txt           || goto :fail
rem English PP-OCRv3 pair - the runtime *default* OCR model (the ch_ pair
rem above is the conformance one). Release first, upstream second: the mirror
rem only exists on tags published after it was added, and both hosts answer
rem 429 under load. Optional either way - the pipeline degrades to the ch_
rem model with a warning when the pair is missing.
call :fetch_opt "%BASE_URL%/ocr_rec_en.onnx"            .models\ocr_rec_en.onnx
if not exist .models\ocr_rec_en.onnx call :fetch_opt "%OCR_EN_URL%" .models\ocr_rec_en.onnx
call :fetch_opt "%BASE_URL%/en_dict.txt"                .models\en_dict.txt
if not exist .models\en_dict.txt call :fetch_opt "%EN_DICT_URL%" .models\en_dict.txt
rem PP-OCRv6 text detector (#429): reads lines the layout model gives no
rem region on bitmap pages. Release first, RapidOCR's hub second; optional -
rem without it OCR stays region-scoped.
call :fetch_opt "%BASE_URL%/ocr_det.onnx"               .models\ocr_det.onnx
if not exist .models\ocr_det.onnx call :fetch_opt "%OCR_DET_URL%" .models\ocr_det.onnx
rem PP-OCRv6 recognizer + dictionary (#570): RapidOCR's / docling's
rem multilingual recognizer, preferred over the PP-OCRv3 pairs when present.
rem Release first, RapidOCR's hub second; optional - the v3 pairs run without it.
call :fetch_opt "%BASE_URL%/ocr_rec_v6.onnx"            .models\ocr_rec_v6.onnx
if not exist .models\ocr_rec_v6.onnx call :fetch_opt "%OCR_REC_V6_URL%" .models\ocr_rec_v6.onnx
call :fetch_opt "%BASE_URL%/ocr_rec_v6_dict.txt"        .models\ocr_rec_v6_dict.txt
if not exist .models\ocr_rec_v6_dict.txt call :fetch_opt "%OCR_REC_V6_DICT_URL%" .models\ocr_rec_v6_dict.txt
call :fetch "%BASE_URL%/encoder.onnx"                   .models\tableformer\encoder.onnx    || goto :fail
call :fetch_opt "%BASE_URL%/encoder.onnx.data"          .models\tableformer\encoder.onnx.data
call :fetch "%BASE_URL%/decoder.onnx"                   .models\tableformer\decoder.onnx    || goto :fail
call :fetch_opt "%BASE_URL%/decoder.onnx.data"          .models\tableformer\decoder.onnx.data
rem True-KV-cache decoder - preferred by the Rust loop when present.
call :fetch_opt "%BASE_URL%/decoder_kv.onnx"            .models\tableformer\decoder_kv.onnx
call :fetch_opt "%BASE_URL%/decoder_kv.onnx.data"       .models\tableformer\decoder_kv.onnx.data
rem Its per-layer-cache graph over the same .data (#871) - preferred over it.
call :fetch_opt "%BASE_URL%/decoder_kv_layers.onnx"     .models\tableformer\decoder_kv_layers.onnx
call :fetch "%BASE_URL%/bbox.onnx"                      .models\tableformer\bbox.onnx       || goto :fail
call :fetch_opt "%BASE_URL%/bbox.onnx.data"             .models\tableformer\bbox.onnx.data
call :fetch_opt "%BASE_URL%/wordmap.json"               .models\tableformer\wordmap.json
call :fetch_opt "%BASE_URL%/picture_classifier.onnx"    .models\picture_classifier.onnx
call :fetch_opt "%BASE_URL%/chunk_tokenizer.json"       .models\chunk\tokenizer.json

rem --- ASR (Whisper tiny) -----------------------------------------------------
rem Release mirror (asr_*) first, Hugging Face second - same order as the .sh
rem twin, so a rate-limited HF doesn't sink the install. A goto guard rather
rem than an "if (...)" block, and each tier on its own line: cmd binds a "||"
rem written after an "if" to the whole if-statement, so a skipped fallback
rem would inherit a stale errorlevel and jump to :fail. The required files
rem are guarded by their own "if not exist ... goto :fail" instead.
if %WITH_ASR%==0 goto :asr_done
call :fetch_opt "%BASE_URL%/asr_encoder_model.onnx"     .models\asr\encoder_model.onnx
if not exist .models\asr\encoder_model.onnx call :fetch "%ASR_BASE_URL%/onnx/encoder_model.onnx" .models\asr\encoder_model.onnx
if not exist .models\asr\encoder_model.onnx goto :fail
call :fetch_opt "%BASE_URL%/asr_decoder_model.onnx"     .models\asr\decoder_model.onnx
if not exist .models\asr\decoder_model.onnx call :fetch "%ASR_BASE_URL%/onnx/decoder_model.onnx" .models\asr\decoder_model.onnx
if not exist .models\asr\decoder_model.onnx goto :fail
call :fetch_opt "%BASE_URL%/asr_vocab.json"             .models\asr\vocab.json
if not exist .models\asr\vocab.json call :fetch "%ASR_BASE_URL%/vocab.json" .models\asr\vocab.json
if not exist .models\asr\vocab.json goto :fail
call :fetch_opt "%BASE_URL%/asr_added_tokens.json"      .models\asr\added_tokens.json
if not exist .models\asr\added_tokens.json call :fetch_opt "%ASR_BASE_URL%/added_tokens.json" .models\asr\added_tokens.json
:asr_done

rem --- INT8 variants (preferred automatically when present; DOCLING_RS_FP32=1 opts out)
if %WITH_INT8%==1 (
  call :fetch_opt "%BASE_URL%/layout_heron_int8.onnx"   .models\layout_heron_int8.onnx
  call :fetch_opt "%BASE_URL%/decoder_int8.onnx"        .models\tableformer\decoder_int8.onnx
  call :fetch_opt "%BASE_URL%/decoder_kv_int8.onnx"     .models\tableformer\decoder_kv_int8.onnx
  call :fetch_opt "%BASE_URL%/decoder_kv_int8.onnx.data" .models\tableformer\decoder_kv_int8.onnx.data
  if exist .models\layout_heron_int8.onnx echo int8 models present - used by default (DOCLING_RS_FP32=1 forces full precision)
)

if not "%MANIFEST%"=="" del "%MANIFEST%" 2>nul
echo done - .models\ populated in %CD%
exit /b 0

:fetch
if %FORCE%==1 goto :fetch_dl
if not exist "%~2" goto :fetch_dl
echo   = %~2 (already present)
exit /b 0
:fetch_dl
echo   ^> %~2
curl -fsSL %CURL_TIMEOUTS% -o "%~2.download" "%~1"
if errorlevel 1 (del "%~2.download" 2>nul & exit /b 1)
call :verify "%~1" "%~2.download" || exit /b 1
move /y "%~2.download" "%~2" >nul
exit /b 0

:fetch_opt
if %FORCE%==1 goto :fetch_opt_dl
if not exist "%~2" goto :fetch_opt_dl
echo   = %~2 (already present)
exit /b 0
:fetch_opt_dl
curl -fsSL %CURL_TIMEOUTS% -o "%~2.download" "%~1" >nul 2>nul
if errorlevel 1 (del "%~2.download" 2>nul & exit /b 0)
rem A hash mismatch on an optional asset is as fatal as on a required one.
call :verify "%~1" "%~2.download" || exit /b 1
move /y "%~2.download" "%~2" >nul
echo   ^> %~2
exit /b 0

rem :verify <url> <downloaded-file> - a release asset against SHA256SUMS.
rem A URL outside %BASE_URL% (an upstream fallback host) is not covered.
:verify
if "%MANIFEST%"=="" exit /b 0
set "VURL=%~1"
set "VNAME=!VURL:%BASE_URL%/=!"
if "!VNAME!"=="!VURL!" exit /b 0
set "WANT="
for /f "tokens=1,2" %%h in ('findstr /e /c:"  !VNAME!" "%MANIFEST%"') do if "%%i"=="!VNAME!" set "WANT=%%h"
if "!WANT!"=="" (
  if /i "%VERIFY%"=="require" (
    del "%~2" 2>nul
    echo error: !VNAME! is not listed in %BASE_URL%/SHA256SUMS ^(DOCLING_RS_MODELS_VERIFY=require^)
    exit /b 1
  )
  echo   ! !VNAME! is not listed in SHA256SUMS - not integrity-checked
  exit /b 0
)
set "GOT="
for /f "skip=1 tokens=*" %%h in ('certutil -hashfile "%~2" SHA256') do if not defined GOT set "GOT=%%h"
set "GOT=!GOT: =!"
if /i not "!GOT!"=="!WANT!" (
  del "%~2" 2>nul
  echo error: !VNAME!: sha256 mismatch ^(expected !WANT!, got !GOT!^) - a corrupted download or a tampered mirror; re-run, and only if it repeats set DOCLING_RS_MODELS_VERIFY=off to bypass the check at your own risk
  exit /b 1
)
exit /b 0

:fail
echo error: download failed - check network access and %BASE_URL%
exit /b 1
