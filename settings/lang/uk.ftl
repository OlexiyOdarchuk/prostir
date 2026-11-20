# Вікно налаштувань Prostir, українською. Чого тут бракує, береться з en.ftl.

language-name = Українська

## Сторінки

nav-overview = Огляд
nav-monitors = Монітори
nav-headsets = Шоломи
nav-sound = Звук
nav-look = Вигляд
nav-first-run = Перший запуск

## Рядок стану під сторінками

side-loading = Підключаюся до Prostir
side-connected = { $name } підключено
side-taken-off = { $name } знято
side-connecting = Підключаю { $name }
side-waiting = Чекаю на шолом
side-failed = Монітор не додано
side-not-running = Prostir не запущено
side-no-headsets = Шоломів ще немає

## Огляд

ov-title = Огляд
ov-connected-title = Шолом підключено
ov-connected-wifi = { $name } через Wi-Fi. Моніторів у шоломі: { $count }.
ov-connected-usb = { $name } через кабель USB. Моніторів у шоломі: { $count }.
ov-taken-off-title = Шолом знято
ov-taken-off-body = { $name } на паузі: монітори чекають, поки шолом знову вдягнуть.
ov-battery = заряд шолома
ov-charging = заряджається
ov-fact-sound = Звук ПК
ov-fact-mic = Мікрофон шолома
ov-fact-profile = Профіль
ov-profile-none = Немає
ov-connecting-title = Підключаю { $name }
ov-connecting-body = Шолом знайшов цей ПК. Встановлюю захищене з'єднання.
ov-waiting-title = Чекаю на шолом
ov-waiting-body = Відкрийте Prostir у шоломі поруч із цим ПК. Шолом знайде ПК сам, якщо вони в одній мережі.
ov-failed-title = Монітор не додано
ov-failed-body = { $name }: { $reason } Решта моніторів працює.
ov-retry = Спробувати ще раз
ov-open-monitors = Перейти до моніторів
ov-no-headsets-title = Сполучіть перший шолом
ov-no-headsets-body = Цей ПК ще не знає жодного шолома. Перший запуск проведе через усе за чотири кроки.
ov-no-headsets-go = Почати перший запуск
ov-pair-new = Сполучити новий шолом
ov-problems = Потребує уваги
ov-monitors = Монітори в шоломі
ov-monitors-empty = Монітори з'являться тут, коли шолом підключиться.
ov-pc = Цей ПК
ov-pc-name = Назва в мережі
ov-pc-desktop = Стільниця
ov-autostart = Запускати разом із системою
ov-autostart-desc = Prostir чекатиме на шолом після входу в систему. Вікно при цьому не відкривається.

## Стан монітора

state-streaming = Передається
state-starting = Запускається
state-failed = Не додано
state-absent = Не під'єднано
state-shown = Показано в шоломі
state-hidden = Приховано

## Короткі значення

sound-in-headset = У шоломі
sound-on-pc = На ПК
on = Увімкнено
off = Вимкнено
percent = { $n } %

## Prostir не запущено

loading-title = Підключаюся до Prostir
nr-title = Prostir не запущено
nr-body = Налаштування беруться з Prostir, що працює у фоні. Запустіть його, і вікно оновиться само.
nr-start = Запустити Prostir
nr-starting = Запускаю Prostir
nr-failed-title = Prostir не запустився
nr-failed-body = Запустіть prostir у терміналі, щоб побачити, що йому заважає.
nr-no-program = Програми prostir не знайдено. Перевірте, чи встановлено Prostir.
nr-no-bus = Немає сеансової шини D-Bus, тож вікно не може зв'язатися з Prostir.

## Монітори

mon-title = Монітори
mon-sub = Так їх бачить стільниця ПК: миша переходить між моніторами там, де вони стикаються.
mon-flat = Пласка
mon-arc = Навколо вас
mon-hint-flat = Перетягніть монітор або виберіть його й рухайте стрілками. Він прилипне до сусіда.
mon-hint-arc = Так монітори стоять навколо вас. Потягніть порожнє місце, щоб повернути вид.
mon-empty = Поки немає моніторів, які можна показати.
mon-selected = Вибраний монітор
mon-none = Виберіть монітор на карті.
mon-size = { $w } × { $h }
mon-size-scaled = { $w } × { $h }, масштаб { $scale } %
mon-scale = Масштаб
mon-main = Головний монітор
mon-main-desc = Тут панель і нові вікна.
mon-remove = Прибрати монітор
mon-in-headset = У шоломі
mon-shown = Показано
mon-hidden = Приховано
mon-laptop = Екран ноутбука
mon-pc-screen = Екран ПК
mon-add = Додати монітор
mon-add-desc = Новий монітор стане праворуч від останнього.
mon-add-full = Більше моніторів не вміщається: у шоломі вже { $max }.
mon-add-offline = Додавати монітори можна, коли шолом підключено.
mon-failed = Не додано: { $reason }

## Шоломи

hs-title = Шоломи
hs-sub = Шоломи, яким цей ПК довіряє. Новому шолому код потрібен лише раз.
hs-now = Підключено зараз
hs-last = Востаннє { $when }
hs-never = Ще не підключався
ago-now = щойно
ago-minutes = { $n } хв тому
ago-hours = { $n } год тому
ago-days = { $n ->
    [one] { $n } день тому
    [few] { $n } дні тому
   *[many] { $n } днів тому
}
hs-forget = Забути
hs-forget-named = Забути { $name }
hs-forget-ask = Забути { $name }? Щоб підключитися знову, йому знадобиться код.
hs-cancel = Скасувати
hs-empty-title = Ще немає сполучених шоломів
hs-empty-body = Відкрийте Prostir у шоломі поруч із цим ПК і сполучіть їх кодом.

## Сполучення

pair-title = Сполучення відкрито
pair-left = ще { $time }
pair-wait = Відкрийте Prostir у новому шоломі поруч із цим ПК. Шолом знайде ПК і покаже код або попросить код звідси.
pair-waiting = Чекаю на новий шолом
pair-type = { $name } показує код. Введіть його тут:
pair-pc-code = Введіть цей код у шоломі:
pair-code = Код із шолома
pair-go = Сполучити
pair-checking = Перевіряю код
pair-stop = Зупинити сполучення
pair-wrong = Код не підійшов. Перевірте цифри на шоломі й введіть ще раз.
pair-done = { $name } сполучено. Далі він підключатиметься сам.
pair-expired = Час сполучення минув. Відкрийте його знову, коли шолом буде поруч.
pair-reopen = Відкрити знову
pair-close = Закрити
pair-firewall = Шолом не бачить ПК? Дозвольте на брандмауері порти 47830/tcp і 47831/udp.

## Звук

snd-title = Звук
snd-sub = Звук ПК і мікрофон шолома.
snd-where = Де грає звук ПК
snd-where-desc = У шоломі: колонки ПК мовчать, поки шолом підключено.
snd-volume = Гучність
snd-volume-desc = Та сама, що на кнопках шолома і в системі.
snd-volume-off = Гучність змінюється, коли шолом підключено.
snd-mic = Мікрофон шолома на ПК
snd-mic-desc = Програми на ПК бачать його як мікрофон «Prostir».
snd-mic-ready = Мікрофон «Prostir» готовий
snd-mic-off = Вимкнено: програми на ПК мікрофона шолома не бачать
snd-mic-waiting = Мікрофон з'явиться, коли шолом підключиться.
snd-mic-not-allowed = Шолом не дозволив мікрофон. Дозвольте його в шоломі: Налаштування, Програми, Prostir, Дозволи.
snd-mic-failed = Мікрофон не вдалося створити: { $reason }

## Вигляд

look-title = Вигляд
look-sub = Тема, палітра й мова цього вікна.
look-theme = Тема
look-system = Як у системі
look-light = Світла
look-dark = Темна
look-now-light = Зараз у системі світла.
look-now-dark = Зараз у системі темна.
look-now-none = Система не каже, яку тему воліє: показано темну.
look-palette = Палітра
look-palette-ink = Prostir
look-palette-graphite = Нейтральна
look-palette-desc = Prostir має відтінок синього. Нейтральна лише сіра, тож нічого не відтінює монітори.
look-lang = Мова
look-lang-more = Інші мови: покладіть файл .ftl у { $dir }
look-local = Prostir не запущено: вибір діє, поки відкрите це вікно.
look-file = Файл теми
look-file-ok = Застосовано. Зміни у файлі видно одразу, перезапуск не потрібен.
look-file-none = Файлу немає: діє вбудована тема. Створіть його, щоб змінити кольори чи розміри.
look-file-issues = Ці значення не прочитано, замість них вбудовані:
look-file-contrast = Замалий контраст тексту:
look-create = Створити файл
look-open = Відкрити теку
issue-not-toml = Файл не читається як TOML: { $why }
issue-format = format = { $value }: це новіший вигляд файлу, тож його не використано зовсім.
issue-bad = { $at } = { $value } тут не підходить
issue-unknown = { $at }: такого ключа немає
issue-font = { $at }: шрифт не читається ({ $why })
contrast-line = { $scheme }: { $fg } на { $bg } дає { $ratio }:1, треба { $need }:1
scheme-light = Світла
scheme-dark = Темна

## Перший запуск

fr-title = Перший запуск
fr-step = Крок { $n } з { $total }
fr-back = Назад
fr-next = Далі
fr-done = Готово
fr-1-title = Установіть Prostir у шоломі
fr-1-body = Увімкніть на шоломі режим розробника, підключіть його кабелем USB і встановіть файл prostir.apk з тієї самої версії, що й на ПК.
fr-1-link = Відкрити сторінку версій
fr-2-title = Відкрийте Prostir у шоломі поруч із цим ПК
fr-2-body = Шолом і ПК мають бути в одній мережі Wi-Fi. Шолом знайде цей ПК сам.
fr-2-looking = Шукаю шолом у мережі
fr-2-found = { $name } бачить цей ПК
fr-3-title = Сполучіть кодом
fr-3-body = Код потрібен один раз: далі цей шолом підключатиметься сам.
fr-4-title = Запускати Prostir разом із системою?
fr-4-body = Тоді шолом знаходитиме ПК одразу після входу. Без вікон і без зайвої роботи: поки шолома немає, Prostir лише чекає.

## Повідомлення після дії

notice-failed = Не вдалося: { $why }
notice-timeout = Prostir не відповів вчасно. Спробуйте ще раз.
notice-not-running = Prostir не запущено, тож зміну не збережено.
notice-broken = Prostir відповів щось незрозуміле для цього вікна: { $why }
notice-close = Закрити повідомлення
