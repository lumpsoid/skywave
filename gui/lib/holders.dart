import 'package:skywave_gui/crop/crop_lines_holder.dart';
import 'package:skywave_gui/effects/effects.dart';
import 'package:skywave_gui/link/link_holder.dart';
import 'package:skywave_gui/screen/screen_holder.dart';
import 'package:skywave_gui/session/search_holder.dart';
import 'package:skywave_gui/session/shell_holder.dart';
import 'package:skywave_gui/session/timeline_holder.dart';
import 'package:skywave_gui/workdir/workdir_text_holder.dart';

/// One holder per Rust signal, created once at start and kept for the
/// app's lifetime. Each subscribes in [connect], before `Boot` goes out,
/// so nothing Rust sends after boot is missed.
class Holders {
  Holders({required EffectSink effects}) : effects = EffectsListener(effects);

  final link = LinkHolder();
  final screen = ScreenHolder();
  final timeline = TimelineHolder();
  final shell = ShellHolder();
  final search = SearchHolder();
  final cropLines = CropLinesHolder();
  final workdirText = WorkdirTextHolder();
  final EffectsListener effects;

  void connect() {
    link.connect();
    screen.connect();
    timeline.connect();
    shell.connect();
    search.connect();
    cropLines.connect();
    workdirText.connect();
    effects.connect();
  }
}
